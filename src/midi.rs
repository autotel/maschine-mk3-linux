//! ALSA sequencer ports.
//!
//! One client with two ports: an output the DAW subscribes to, and an input
//! the DAW can drive for LED and display feedback. PipeWire and JACK both pick
//! ALSA sequencer ports up automatically, so this covers every host on the
//! machine without a second backend.
//!
//! Events are dispatched with `event_output_direct`, which hands the event to
//! the kernel immediately instead of parking it in the client's output queue.
//! Nothing here allocates once the ports are open.

use alsa::seq::{
    Addr, ClientIter, EvCtrl, EvNote, Event, EventType, PortCap, PortInfo, PortIter, PortType,
    PortSubscribe, PortSubscribeIter, QuerySubsType, Seq,
};
use alsa::Direction;
use anyhow::{Context, Result};
use std::ffi::CString;

/// A MIDI message the engine wants to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg {
    /// Note on. `vel` of 0 is sent as a note off, per convention.
    NoteOn { ch: u8, note: u8, vel: u8 },
    /// Note off.
    NoteOff { ch: u8, note: u8, vel: u8 },
    /// Control change.
    Cc { ch: u8, cc: u8, val: u8 },
    /// Polyphonic key pressure.
    PolyAftertouch { ch: u8, note: u8, val: u8 },
    /// Channel pressure.
    ChannelAftertouch { ch: u8, val: u8 },
    /// Program change.
    Program { ch: u8, num: u8 },
    /// Pitch bend, -8192..=8191.
    PitchBend { ch: u8, val: i16 },
    /// Transport start.
    Start,
    /// Transport stop.
    Stop,
    /// Transport continue.
    Continue,
}

/// Open sequencer client owning the driver's virtual ports.
pub struct MidiIo {
    seq: Seq,
    out_port: i32,
    in_port: i32,
    client: i32,
}

impl MidiIo {
    /// Create the client and both ports.
    pub fn open(client_name: &str, out_name: &str, in_name: &str) -> Result<Self> {
        let seq = Seq::open(None, None, true).context("opening ALSA sequencer")?;
        seq.set_client_name(&CString::new(client_name)?)
            .context("naming ALSA sequencer client")?;

        let mut info = PortInfo::empty()?;
        info.set_name(&CString::new(out_name)?);
        info.set_capability(PortCap::READ | PortCap::SUBS_READ);
        info.set_type(PortType::MIDI_GENERIC | PortType::APPLICATION);
        seq.create_port(&info).context("creating output port")?;
        let out_port = info.get_port();

        let mut info = PortInfo::empty()?;
        info.set_name(&CString::new(in_name)?);
        info.set_capability(PortCap::WRITE | PortCap::SUBS_WRITE);
        info.set_type(PortType::MIDI_GENERIC | PortType::APPLICATION);
        seq.create_port(&info).context("creating input port")?;
        let in_port = info.get_port();

        let client = seq.client_id().context("querying ALSA client id")?;

        Ok(Self {
            seq,
            out_port,
            in_port,
            client,
        })
    }

    /// `client:port` of the output, for logging.
    pub fn out_addr(&self) -> (i32, i32) {
        (self.client, self.out_port)
    }

    /// `client:port` of the input, for logging.
    pub fn in_addr(&self) -> (i32, i32) {
        (self.client, self.in_port)
    }

    /// Send one message immediately.
    pub fn send(&self, m: Msg) -> Result<()> {
        let mut ev = build(m);
        ev.set_source(self.out_port);
        ev.set_subs();
        ev.set_direct();
        self.seq
            .event_output_direct(&mut ev)
            .context("event_output_direct")?;
        Ok(())
    }

    /// Blocking-capable input handle for the feedback thread.
    pub fn input(&self) -> Result<alsa::seq::Input<'_>> {
        Ok(self.seq.input())
    }

    /// Poll descriptors for the input port.
    pub fn poll_fds(&self) -> Result<Vec<libc::pollfd>> {
        use alsa::PollDescriptors;
        let n = (&self.seq, Some(Direction::Capture)).count();
        let mut fds = vec![
            libc::pollfd {
                fd: 0,
                events: 0,
                revents: 0
            };
            n
        ];
        (&self.seq, Some(Direction::Capture)).fill(&mut fds)?;
        Ok(fds)
    }

    /// Ask the sequencer to tell us when clients and subscriptions change.
    ///
    /// Without this the driver has no idea whether anything is listening, and
    /// neither does the person using it: a host that lists the port but never
    /// subscribes looks exactly like a driver that is not transmitting. With
    /// it, every connection and disconnection can be reported as it happens,
    /// and a host that starts later can be connected to automatically.
    pub fn watch_announcements(&self) -> Result<()> {
        let announce = Addr::system_announce();
        self.connect_from(announce.client, announce.port)
            .context("subscribing to the sequencer's announce port")
    }

    /// Our own client id, so announcements about us can be recognised.
    pub fn client_id(&self) -> i32 {
        self.client
    }

    /// The output port number.
    pub fn out_port(&self) -> i32 {
        self.out_port
    }

    /// Name a `client:port` for logging, falling back to the numbers.
    pub fn describe(&self, addr: Addr) -> String {
        describe_addr(&self.seq, addr)
    }

    /// Every sequencer port that could receive what we send.
    ///
    /// Some hosts list a port without ever subscribing to it, which looks
    /// exactly like a driver that is not transmitting. Printing the candidates
    /// makes the difference visible, and [`MidiIo::connect_to_matching`] can
    /// make the connection from this side.
    pub fn destinations(&self) -> Vec<(i32, i32, String)> {
        let mut out = Vec::new();
        for client in ClientIter::new(&self.seq) {
            if client.get_client() == self.client || client.get_client() == 0 {
                continue;
            }
            let cname = client.get_name().unwrap_or("?").to_string();
            for port in PortIter::new(&self.seq, client.get_client()) {
                let caps = port.get_capability();
                if !caps.contains(PortCap::WRITE) || !caps.contains(PortCap::SUBS_WRITE) {
                    continue;
                }
                out.push((
                    client.get_client(),
                    port.get_port(),
                    format!("{cname}:{}", port.get_name().unwrap_or("?")),
                ));
            }
        }
        out
    }

    /// Subscribe every destination whose name contains one of `patterns`.
    ///
    /// Returns the names actually connected. Matching is case-insensitive and
    /// on a substring, so `"REAPER"` is enough.
    pub fn connect_to_matching(&self, patterns: &[String]) -> Vec<String> {
        let mut done = Vec::new();
        for (client, port, name) in self.destinations() {
            let lower = name.to_lowercase();
            if !patterns
                .iter()
                .any(|p| !p.is_empty() && lower.contains(&p.to_lowercase()))
            {
                continue;
            }
            match self.connect_to(client, port) {
                Ok(()) => done.push(name),
                Err(e) => eprintln!("[midi] could not connect to {name}: {e:#}"),
            }
        }
        done
    }

    /// Names that must never be connected to automatically.
    ///
    /// `Midi Through` loops whatever it is sent straight back, so wiring our
    /// output to it while our input is also connected builds a feedback loop.
    /// The system client is not a MIDI destination at all.
    ///
    /// `exclude` carries the controller's own name, which keeps the surface
    /// off the hardware's DIN output. Sending every button press out of the
    /// physical MIDI socket is a legitimate thing to want and a surprising
    /// thing to get without asking; `general.connect_to` turns it back on.
    fn is_unsafe_to_autoconnect(name: &str, exclude: &[String]) -> bool {
        let l = name.to_lowercase();
        if l.contains("midi through") || l.starts_with("system:") || l.contains("announce") {
            return true;
        }
        exclude
            .iter()
            .any(|e| !e.is_empty() && l.starts_with(&e.to_lowercase()))
    }

    /// Subscribe anything that looks like a host, and say what was connected.
    ///
    /// This is what makes the driver work without the person using it having
    /// to know that listing a port and subscribing to it are separate steps in
    /// ALSA. Ports that would form a loop are skipped.
    pub fn connect_to_all_hosts(
        &self,
        already: &mut Vec<(i32, i32)>,
        exclude: &[String],
    ) -> Vec<String> {
        let mut done = Vec::new();
        for (client, port, name) in self.destinations() {
            if client == self.client || already.contains(&(client, port)) {
                continue;
            }
            if Self::is_unsafe_to_autoconnect(&name, exclude) {
                continue;
            }
            match self.connect_to(client, port) {
                Ok(()) => {
                    already.push((client, port));
                    done.push(name);
                }
                // Already subscribed is not a failure; note it so we stop
                // trying on every rescan.
                Err(_) => already.push((client, port)),
            }
        }
        done
    }

    /// Subscribe `dest` to our output port.
    pub fn connect_to(&self, dest_client: i32, dest_port: i32) -> Result<()> {
        let sub = PortSubscribe::empty()?;
        sub.set_sender(Addr {
            client: self.client,
            port: self.out_port,
        });
        sub.set_dest(Addr {
            client: dest_client,
            port: dest_port,
        });
        self.seq
            .subscribe_port(&sub)
            .with_context(|| format!("subscribing {dest_client}:{dest_port} to our output"))?;
        Ok(())
    }

    /// Subscribe our input port to `src`, so a DAW's output drives our LEDs.
    pub fn connect_from(&self, src_client: i32, src_port: i32) -> Result<()> {
        let sub = PortSubscribe::empty()?;
        sub.set_sender(Addr {
            client: src_client,
            port: src_port,
        });
        sub.set_dest(Addr {
            client: self.client,
            port: self.in_port,
        });
        self.seq
            .subscribe_port(&sub)
            .with_context(|| format!("subscribing to {src_client}:{src_port}"))?;
        Ok(())
    }
}

/// Name a `client:port` for logging, falling back to the numbers.
fn describe_addr(seq: &Seq, addr: Addr) -> String {
    for client in ClientIter::new(seq) {
        if client.get_client() != addr.client {
            continue;
        }
        let cname = client.get_name().unwrap_or("?").to_string();
        for port in PortIter::new(seq, addr.client) {
            if port.get_port() == addr.port {
                return format!("{cname}:{}", port.get_name().unwrap_or("?"));
            }
        }
        return cname;
    }
    format!("{}:{}", addr.client, addr.port)
}

/// Who is actually subscribed on either side of `addr` right now.
///
/// A sequencer port can be listed by a host without that host ever
/// subscribing to it -- from the driver's side that looks identical to a
/// working connection, since sending to an unsubscribed port fails silently.
/// This is the other half of the picture: `listening` true asks who receives
/// what `addr` sends (is anything actually listening on our output?);
/// false asks who feeds `addr` (what is driving our LEDs?).
///
/// Opens its own transient sequencer connection rather than reusing the
/// driver's, so a GUI polling this never touches the real-time thread's `Seq`
/// handle from another thread.
pub fn query_subscribers(addr: (i32, i32), listening: bool) -> Vec<String> {
    let Ok(seq) = Seq::open(None, None, false) else {
        return Vec::new();
    };
    let target = Addr {
        client: addr.0,
        port: addr.1,
    };
    let qtype = if listening {
        QuerySubsType::READ
    } else {
        QuerySubsType::WRITE
    };
    PortSubscribeIter::new(&seq, target, qtype)
        .map(|s| {
            let other = if listening { s.get_dest() } else { s.get_sender() };
            describe_addr(&seq, other)
        })
        .collect()
}

fn build(m: Msg) -> Event<'static> {
    match m {
        Msg::NoteOn { ch, note, vel } if vel == 0 => build(Msg::NoteOff { ch, note, vel: 0 }),
        Msg::NoteOn { ch, note, vel } => Event::new(
            EventType::Noteon,
            &EvNote {
                channel: ch,
                note,
                velocity: vel,
                off_velocity: 0,
                duration: 0,
            },
        ),
        Msg::NoteOff { ch, note, vel } => Event::new(
            EventType::Noteoff,
            &EvNote {
                channel: ch,
                note,
                velocity: 0,
                off_velocity: vel,
                duration: 0,
            },
        ),
        Msg::Cc { ch, cc, val } => Event::new(
            EventType::Controller,
            &EvCtrl {
                channel: ch,
                param: cc as u32,
                value: val as i32,
            },
        ),
        Msg::PolyAftertouch { ch, note, val } => Event::new(
            EventType::Keypress,
            &EvNote {
                channel: ch,
                note,
                velocity: val,
                off_velocity: 0,
                duration: 0,
            },
        ),
        Msg::ChannelAftertouch { ch, val } => Event::new(
            EventType::Chanpress,
            &EvCtrl {
                channel: ch,
                param: 0,
                value: val as i32,
            },
        ),
        Msg::Program { ch, num } => Event::new(
            EventType::Pgmchange,
            &EvCtrl {
                channel: ch,
                param: 0,
                value: num as i32,
            },
        ),
        Msg::PitchBend { ch, val } => Event::new(
            EventType::Pitchbend,
            &EvCtrl {
                channel: ch,
                param: 0,
                value: val as i32,
            },
        ),
        Msg::Start => Event::new(EventType::Start, &()),
        Msg::Stop => Event::new(EventType::Stop, &()),
        Msg::Continue => Event::new(EventType::Continue, &()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopbacks_and_the_devices_own_port_are_not_auto_connected() {
        let exclude = vec!["Maschine MK3".to_string()];
        // A hardware loopback would feed our own output straight back in.
        assert!(MidiIo::is_unsafe_to_autoconnect(
            "Midi Through:Midi Through Port-0",
            &exclude
        ));
        // The controller's own DIN socket: legitimate, but not a default.
        assert!(MidiIo::is_unsafe_to_autoconnect(
            "Maschine MK3:Maschine MK3 MIDI 1",
            &exclude
        ));
        assert!(MidiIo::is_unsafe_to_autoconnect("System:Announce", &exclude));
        // An actual host is exactly what we want to connect.
        assert!(!MidiIo::is_unsafe_to_autoconnect("REAPER:MIDI Input 1", &exclude));
        assert!(!MidiIo::is_unsafe_to_autoconnect("Bitwig Studio:in", &exclude));
    }
}
