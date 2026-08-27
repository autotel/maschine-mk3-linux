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
    PortSubscribe, Seq,
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
