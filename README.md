# maschine-mk3-linux

[! note] This software has not been thoroughly tested yet.

A userspace driver that turns the Native Instruments Maschine MK3 into a
configurable MIDI controller on Linux. Pads, knobs, buttons, the encoder, the
touch strip, every LED and both colour screens.

Native Instruments ships no Linux driver. None is needed — the MK3 speaks
standard USB, and the parts that are not class-compliant are a plain HID
interface and a bulk endpoint that takes pixels. This is that, written out.

## What the device actually exposes

The MK3 presents seven USB interfaces. `lsusb -v -d 17cc:1600` shows all of
them; the driver only has to supply two.


| #   | Class                           | What it is                              | Handled by               |
| --- | ------------------------------- | --------------------------------------- | ------------------------ |
| 0-2 | Audio (UAC2)                    | 4 out / 2 in, 24-bit, up to 96 kHz      | `snd-usb-audio` (kernel) |
| 3   | MIDI Streaming                  | the rear DIN MIDI in/out jacks          | `snd-usb-audio` (kernel) |
| 4   | HID                             | pads, buttons, knobs, encoder, all LEDs | **this driver**          |
| 5   | Vendor`0xbd`, "Maschine MK3 BD" | bulk pixel data for both screens        | **this driver**          |
| 6   | DFU                             | firmware update                         | nothing                  |

The audio interface and the DIN jacks already work out of the box on any modern
kernel. See [`audio/README.md`](audio/README.md) for the two configuration
changes worth making to them.

Interface 5 has no kernel driver bound to it, so claiming it needs no unbind
step — just permission on the usbfs node, which the shipped udev rule grants.

## Install

```sh
git clone <this repo> && cd maschine-mk3-linux
./install.sh
```

That builds, installs `mk3d` and `mk3-learn` into `~/.local/bin`, installs the
udev rule (the one step that asks for `sudo`), and installs a systemd user unit.
**Unplug and replug the Maschine afterwards** so the new permissions apply.

Build dependencies are a Rust toolchain and ALSA's headers:

```sh
sudo apt install build-essential libasound2-dev   # Debian / Ubuntu
sudo dnf install @development-tools alsa-lib-devel # Fedora
sudo pacman -S base-devel alsa-lib                 # Arch
```

There is no libusb dependency; USB is spoken directly through usbfs.

## Run

```sh
mk3d                                   # foreground
systemctl --user enable --now maschine-mk3d   # or as a service
```

It creates two ALSA sequencer ports:

```
client 128: 'Maschine MK3'
    0 'Controller Out'   <- subscribe your DAW to this
    1 'Controller In'    <- send here to drive LEDs from the host
```

PipeWire and JACK both pick ALSA sequencer ports up automatically, so these
appear in every host on the machine with no second backend.

## Two files, and why

**`devices/maschine-mk3.toml`** describes the hardware: which bit in the HID
report is Play, which LED slot lights it, where it sits on the panel. None of
it is a preference. It is compiled into the driver, so nothing needs
installing; a copy at `~/.config/maschine-mk3/devices/maschine-mk3.toml`
overrides it.

**`config.toml`** is yours. It names controls the way the panel does and says
what each should send:

```toml
[buttons]
play = "cc 1 118"
mute = { send = "cc 16 37", mode = "toggle" }
```

Keeping them apart means a correction to the hardware map cannot disturb your
mapping, and that supporting another NI controller is a matter of writing a
profile rather than changing the driver -- see
[`docs/porting.md`](docs/porting.md).

A name the device does not have is refused with a suggestion, rather than
silently ignored:

```
buttons.plya: no control called that; did you mean play?
```

## Presets

A preset is a whole set of settings under a name, so "my drum kit" and "my
mixer layout" are two files rather than an evening of re-editing.

```sh
mk3d --list-presets        # what is available, and which is loaded
mk3d --preset drums        # load one and run
mk3d --save-preset my-kit  # save the current settings under a name
```

Five ship with the driver, compiled in so they work with nothing installed:
`default`, `drums`, `keys`, `mixer` and `minimal`. A file of your own with the
same name shadows a built-in one, so you can adjust `drums` to taste and get
the original back by deleting your copy.

They are ordinary config files, so **sharing one means sending a file**. An
imported preset is checked against the device before anything is written, and
loading one keeps the previous config as `config.toml.prev`. See
[`presets/README.md`](presets/README.md).

## Configure

Two ways, same file:

```sh
mk3-gui                                      # a window
$EDITOR ~/.config/maschine-mk3/config.toml   # text
```

`mk3-gui` shows the panel as a map, with every control where your hand expects
it. Press something on the hardware and it lights up on screen and selects
itself, so a button is configured by pressing it rather than by finding its
name in a list. It starts the driver if it is not already running; the driver
never opens a window.

The two are separate processes on purpose: the driver holds a real-time input
thread, and a window being resized must not be able to interfere with it. They
talk over a Unix socket.

There is also a browser page, off by default, for configuring a machine you are
only logged into remotely -- set `general.gui_port` to enable it.

The file is watched. Save it and the running driver picks the change up — no
restart, no dropped notes. A file that fails to parse or validate is reported
on stderr and **ignored**, so a typo mid-session cannot take the controller
down. The GUI reads and writes that same file, including a raw TOML pane for
anything the forms do not cover.

The starter file that gets written on first run is commented throughout.

### A taste of it

```toml
[pads]
channel = 10
notes = [48, 49, 50, 51, 44, 45, 46, 47, 40, 41, 42, 43, 36, 37, 38, 39]
curve = "soft"              # linear | soft | hard | fixed
aftertouch = "poly"         # off | poly | channel
threshold = 200             # ignore crosstalk from neighbouring pads

[knobs]
channel = 1
ccs = [16, 17, 18, 19, 20, 21, 22, 23]
mode = "accumulate"         # endless encoders, clamped rather than wrapping
travel = 1000               # raw units for a full sweep

# --- Transport ---------------------------------------------------
play                  = "cc 16 45"
rec                   = { send = "cc 16 46", mode = "toggle" }
stop                  = "cc 16 47"
```

The button section is grouped and headed the way the panel is -- transport,
group buttons, what the screens show -- so finding a control means looking
where your hand would.

## Mapping your unit's buttons

The report descriptor says there are 80 button bits and 103 LED slots. It does
not say which physical button is which — that is only discoverable by pressing
things. `mk3-learn` does it interactively and writes the answers into your
config:

```sh
mk3-learn buttons     # press a button, name it, repeat
mk3-learn leds 16 62  # step through the button LED slots one at a time
```

`buttons` accepts, at each prompt:


| input     | effect                                 |
| --------- | -------------------------------------- |
| `play`    | record the bit under that name         |
| `play 21` | record it with LED slot 21 as well     |
| *enter*   | skip this press, wait for the next     |
| `45`      | use bit 45 instead of the one detected |
| `list`    | show what has been mapped so far       |
| `done`    | finish                                 |

Each name is written to the config the moment you enter it, so a session can be
stopped with ctrl-c and resumed later without losing anything. Only the
`[button.*]` tables are rewritten — the comments explaining every other setting
are left exactly as they were.

The pad and touch strip slots are already known and set in the shipped config,
along with the sixteen button slots listed in
[`docs/hardware-map.md`](docs/hardware-map.md), so `leds` only has slots 16-61 left to
identify.

Other subcommands, useful when something is not behaving:

```sh
mk3-learn watch          # every HID event, decoded, as it happens
mk3-learn info           # the device's own feature reports
mk3-learn palette        # the built-in colour ramp, as RGB
mk3-learn test-display   # gradient on both screens, then an LED sweep
```

## Latency

The design puts one `SCHED_FIFO` thread on a single `poll()` covering both the
HID node and the ALSA sequencer input. That thread owns the mapping engine
outright, so a pad strike becomes a MIDI event with no lock, no allocation and
no context switch. LED and screen updates are handed to a second thread through
`try_lock` snapshots — a busy screen can never delay a note.


| stage                      | cost       |
| -------------------------- | ---------- |
| pad strike to HID report   | up to 1 ms |
| parse, map, dispatch       | a few µs  |
| ALSA sequencer to the host | µs        |

The 1 ms floor is the interrupt endpoint's polling interval (`bInterval = 1` at
USB high speed). It is a property of the hardware and cannot be tuned away —
NI's own driver has the same floor.

Real-time scheduling needs `rtprio` headroom, which on most audio-oriented
distributions comes from membership of the `audio` group:

```sh
sudo usermod -aG audio "$USER"    # then log out and back in
```

Without it the driver still runs, one line on stderr says so, and scheduler
jitter is the only cost.

## LEDs

103 slots: 62 buttons, then the 25 touch strip LEDs, then the 16 pads. The pad
and strip positions are set correctly out of the box; the per-button map and
the three colour encodings are written up in
[`docs/hardware-map.md`](docs/hardware-map.md).

The touch strip works as a meter following your finger. If it fills from the
wrong end, flip `touchstrip.led_reversed`.

**Pads are numbered in reading order** — HID pad 0 is the *top*-left pad, the
one silkscreened 13. `pads.notes` is indexed by HID number, and the shipped
default is transposed so the bottom-left pad plays the lowest note.

## What the screens show

By default: the eight knob values, four per screen, with the CC each one sends,
plus a header. `display.title` sets the left header. Rendering is dirty-row
tracked, so a single knob move retransmits about 30 KB rather than 261 KB.

Brightness and contrast are settable per screen (`[display]`), through the
device's own feature reports.

## Troubleshooting

**"no hidraw node for 17CC:1600"** — the device is not plugged in, or udev has
not been reloaded. `lsusb | grep 17cc` to check, then replug.

**"opening ... Permission denied"** — the udev rule is not installed or the
device was not replugged after installing it. `ls -l /dev/hidraw*` should show
a `+` at the end of the permissions, meaning an ACL is granting you access.

**Screens stay dark, LEDs work** — something else has claimed USB interface 5.
Only one process can. Check for a second `mk3d`, or a `mk3-learn test-display`
still running.

**The host lists the port but receives nothing** — it is almost always that the
host has not subscribed. Listing a port and connecting to it are separate steps
in ALSA, and a host that skips the second looks exactly like a driver that is
not transmitting.

```sh
mk3d --list-ports          # what could receive our output
aseqdump -p 128:0          # prove the driver is sending
```

If `aseqdump` shows events, the driver is fine. Connect from this side by
naming the host in the config:

```toml
[general]
connect_to = ["REAPER"]
```

or by hand, once: `aconnect 128:0 <client>:<port>`.

If the host does not appear in `--list-ports` at all, it is not using the ALSA
sequencer -- it is on JACK, and PipeWire's bridge exposes our port there
instead. Connect it in the JACK graph (`qpwgraph`, `Carla`, or `jack_connect`).

**Pads trigger their neighbours** — raise `pads.threshold`. The device's own
per-pad calibration is readable with `mk3-learn info` (reports `0xda` and
`0xdb`) if you want to see how much headroom each pad has.

## Layout of the source

```
src/
  device.rs         finding and opening the hidraw node; feature reports
  hid.rs            decoding input reports 0x01 and 0x02
  leds.rs           the 103-slot LED surface and the colour palette
  display/          the two screens: bulk protocol, framebuffer, fonts
  midi.rs           ALSA sequencer ports
  config.rs         the TOML schema and its validation
  engine.rs         control events in, MIDI out; pure and unit-tested
  ui.rs             what gets drawn on the screens
  gui.rs            the configuration web interface
  rt.rs             SCHED_FIFO and mlockall
  profile.rs        the device description: bits, LED slots, panel positions
  ipc.rs            the control socket the configuration app talks over
  bin/mk3d.rs       the daemon: threads, poll loop, hot reload
  bin/mk3_learn.rs  interactive hardware discovery
  bin/mk3_gui.rs    the configuration window
  preset.rs         named sets of settings, and the shipped ones
devices/
  maschine-mk3.toml the hardware map, as data
presets/
  *.toml            ready-made settings
```

`cargo test` covers the parts that can be tested without hardware: report
decoding, the mapping engine, config validation and the compact action syntax.

## Credit

The display command format was worked out by GoaSkin for
[qKontrol](https://github.com/GoaSkin/qKontrol), which drives the same NI
display engine on the Komplete Kontrol MK2. The pad event triplet format
matches r00tman's
[maschine-mikro-mk3-driver](https://github.com/r00tman/maschine-mikro-mk3-driver).
Everything specific to the MK3 — the report layouts, the LED banks, the feature
reports — was read off this device's own descriptors.

## Licence

GPL-3.0-or-later.
