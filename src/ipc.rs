//! The control socket the configuration app talks to.
//!
//! A Unix socket carrying newline-delimited JSON, one line per message. It
//! exists so the GUI can be a separate process: the driver keeps its
//! real-time input thread and the GUI can be started, closed and restarted
//! without touching it.
//!
//! Two things flow over it. The driver broadcasts what the hardware is doing,
//! which is what lets the GUI put the cursor on the control you just pressed.
//! The GUI sends config, which the driver validates, persists and applies.
//!
//! Broadcasting is deliberately lossy: a client that stops reading is dropped
//! rather than allowed to block the thread feeding it, because nothing about
//! a configuration window should be able to stall the driver.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Something the hardware did, sent to every connected client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Event {
    /// A button changed state.
    Button {
        /// HID bit index.
        bit: usize,
        /// True on press.
        down: bool,
    },
    /// A pad was struck or released.
    Pad {
        /// HID pad index, 0 is the top-left pad.
        pad: u8,
        /// True while sounding.
        down: bool,
        /// Velocity or pressure, 0..=127.
        value: u8,
    },
    /// A knob moved. `value` is what was transmitted, not the raw position.
    Knob {
        /// Knob index, 0..8.
        knob: usize,
        /// Transmitted CC value.
        value: u8,
    },
    /// The 4-D encoder turned.
    Encoder {
        /// Transmitted CC value.
        value: u8,
    },
    /// The touch strip moved.
    Strip {
        /// Transmitted CC value.
        value: u8,
    },
}

/// A request from the configuration app.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Request {
    /// Send back the config currently in force.
    GetConfig,
    /// Validate, persist and apply this config.
    SetConfig {
        /// The whole file, as TOML.
        toml: String,
    },
    /// Send back the device profile in force.
    GetProfile,
    /// List every preset available.
    ListPresets,
    /// Replace the live config with a preset, and apply it.
    LoadPreset {
        /// Preset name.
        name: String,
    },
    /// Save the live config as a named preset.
    SavePreset {
        /// Preset name.
        name: String,
        /// One line saying what it is for.
        description: String,
    },
    /// Delete a user preset.
    DeletePreset {
        /// Preset name.
        name: String,
    },
    /// Write someone else's preset into the user's directory.
    ImportPreset {
        /// Name to file it under.
        name: String,
        /// The file's contents.
        toml: String,
    },
    /// Ask for a reply, to check the driver is alive.
    Ping,
}

/// A reply to a [`Request`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Reply {
    /// The current config.
    Config {
        /// The whole file, as TOML.
        toml: String,
        /// Where it lives on disk.
        path: String,
    },
    /// The request succeeded.
    Ok,
    /// The request failed, with a message fit to show a user.
    Error {
        /// What went wrong.
        message: String,
    },
    /// The device profile.
    Profile {
        /// The whole file, as TOML.
        toml: String,
        /// Where it lives on disk, or "(built in)".
        path: String,
    },
    /// The presets available.
    Presets {
        /// Every preset, sorted by name.
        entries: Vec<PresetEntry>,
        /// Where user presets live.
        dir: String,
        /// Which one the live config came from, if it says.
        active: Option<String>,
    },
    /// Reply to [`Request::Ping`].
    Pong {
        /// Driver version.
        version: String,
    },
}

/// One preset, as listed over the socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetEntry {
    /// The name used to load it.
    pub name: String,
    /// One line describing what it is for.
    pub description: String,
    /// Whether it is compiled into the driver rather than a user file.
    pub builtin: bool,
    /// Whether a user file of this name shadows a built-in one.
    pub shadows_builtin: bool,
}

/// Anything the driver sends: an unsolicited event or a reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Outbound {
    /// An unsolicited hardware event.
    Event(Event),
    /// A reply to a request.
    Reply(Reply),
}

/// Default socket path, under the user's runtime directory.
pub fn socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("maschine-mk3.sock")
}

/// The set of connected clients, and the means to broadcast to them.
#[derive(Clone, Default)]
pub struct Broadcaster {
    clients: Arc<Mutex<Vec<UnixStream>>>,
}

impl Broadcaster {
    /// A broadcaster with nobody listening yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether anyone is connected. Callers use this to skip serialising
    /// events nobody will read.
    pub fn has_clients(&self) -> bool {
        self.clients.lock().map(|c| !c.is_empty()).unwrap_or(false)
    }

    fn add(&self, s: UnixStream) {
        if let Ok(mut c) = self.clients.lock() {
            c.push(s);
        }
    }

    /// Send one event to every client, dropping any that has gone away.
    ///
    /// Called from the driver's input thread, so it never blocks: the socket
    /// is non-blocking and a client that cannot keep up loses events rather
    /// than delaying a pad hit.
    pub fn send(&self, ev: &Event) {
        let Ok(mut clients) = self.clients.try_lock() else {
            return;
        };
        if clients.is_empty() {
            return;
        }
        let Ok(mut line) = serde_json::to_string(&Outbound::Event(ev.clone())) else {
            return;
        };
        line.push('\n');
        clients.retain_mut(|c| c.write_all(line.as_bytes()).is_ok());
    }
}

/// Serve the control socket until `running` clears.
///
/// `get` returns the config in force; `set` validates, persists and applies.
pub fn serve<G, S, P, R>(
    path: &std::path::Path,
    broadcaster: Broadcaster,
    running: Arc<AtomicBool>,
    get: G,
    set: S,
    profile: P,
    presets: R,
) -> Result<()>
where
    G: Fn() -> (String, String) + Send + Sync + 'static,
    S: Fn(String) -> Result<()> + Send + Sync + 'static,
    P: Fn() -> (String, String) + Send + Sync + 'static,
    R: Fn(Request) -> Reply + Send + Sync + 'static,
{
    // A socket left behind by a crashed driver would block the bind, and it
    // cannot be connected to, so removing it is safe.
    if path.exists() && UnixStream::connect(path).is_err() {
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("binding {}", path.display()))?;
    listener
        .set_nonblocking(true)
        .context("setting the control socket non-blocking")?;
    eprintln!("[ipc] listening on {}", path.display());

    let get = Arc::new(get);
    let set = Arc::new(set);
    let profile = Arc::new(profile);
    let presets = Arc::new(presets);

    while running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let Ok(writer) = stream.try_clone() else {
                    continue;
                };
                // Events are pushed, not polled, so the broadcast half must
                // never block on a client that has stopped reading.
                let _ = writer.set_nonblocking(true);
                broadcaster.add(writer);

                let get = get.clone();
                let set = set.clone();
                let profile = profile.clone();
                let presets = presets.clone();
                let running = running.clone();
                std::thread::Builder::new()
                    .name("mk3-ipc-client".into())
                    .spawn(move || client_loop(stream, get, set, profile, presets, running))
                    .ok();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("[ipc] accept failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
    let _ = std::fs::remove_file(path);
    Ok(())
}

fn client_loop<G, S, P, R>(
    stream: UnixStream,
    get: Arc<G>,
    set: Arc<S>,
    profile: Arc<P>,
    presets: Arc<R>,
    running: Arc<AtomicBool>,
) where
    G: Fn() -> (String, String),
    S: Fn(String) -> Result<()>,
    P: Fn() -> (String, String),
    R: Fn(Request) -> Reply,
{
    // The request half blocks; only the broadcast half is non-blocking.
    let _ = stream.set_nonblocking(false);
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        if !running.load(Ordering::Relaxed) {
            return;
        }
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Request>(&line) {
            Err(e) => Reply::Error {
                message: format!("bad request: {e}"),
            },
            Ok(Request::Ping) => Reply::Pong {
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            Ok(Request::GetConfig) => {
                let (toml, path) = get();
                Reply::Config { toml, path }
            }
            Ok(Request::GetProfile) => {
                let (toml, path) = profile();
                Reply::Profile { toml, path }
            }
            Ok(Request::SetConfig { toml }) => match set(toml) {
                Ok(()) => Reply::Ok,
                Err(e) => Reply::Error {
                    message: format!("{e:#}"),
                },
            },
            // Preset handling needs the config path and the device profile,
            // so it is delegated to the driver rather than reimplemented here.
            Ok(other) => presets(other),
        };
        let Ok(mut text) = serde_json::to_string(&Outbound::Reply(reply)) else {
            return;
        };
        text.push('\n');
        if out.write_all(text.as_bytes()).is_err() {
            return;
        }
    }
}

/// Client side: a connection to a running driver.
pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    /// Connect to the driver's control socket.
    pub fn connect(path: &std::path::Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("connecting to {}", path.display()))?;
        let reader = BufReader::new(stream.try_clone().context("dup socket")?);
        Ok(Self { stream, reader })
    }

    /// Send a request. The reply arrives through [`Client::poll`].
    pub fn send(&mut self, req: &Request) -> Result<()> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        self.stream.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Read whatever has arrived without blocking.
    pub fn poll(&mut self) -> Vec<Outbound> {
        let mut out = Vec::new();
        // Draining under a non-blocking socket keeps the UI thread responsive
        // no matter how fast the hardware is producing events.
        if self.stream.set_nonblocking(true).is_err() {
            return out;
        }
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if let Ok(msg) = serde_json::from_str::<Outbound>(line.trim()) {
                        out.push(msg);
                    }
                }
                Err(_) => break,
            }
        }
        out
    }

    /// Send a request and wait up to `timeout` for its reply.
    pub fn request(&mut self, req: &Request, timeout: std::time::Duration) -> Result<Reply> {
        self.stream.set_nonblocking(false)?;
        self.stream.set_read_timeout(Some(timeout))?;
        self.send(req)?;
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).context("reading reply")?;
            if n == 0 {
                anyhow::bail!("the driver closed the connection");
            }
            // Events can arrive between the request and its reply.
            if let Ok(Outbound::Reply(r)) = serde_json::from_str::<Outbound>(line.trim()) {
                return Ok(r);
            }
        }
    }
}
