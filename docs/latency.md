# Diagnosing latency

"It feels laggy" can mean five different things depending on which stage of
the chain is slow. This walks the whole path from pad strike to sound, in
order, with the command that tells you the actual number at each stage rather
than a guess.

```
pad strike -> HID report -> driver -> ALSA sequencer -> host routing -> synth -> audio buffer -> speakers
     |____________________|                                                |__________________|
      this driver's budget                                              usually the real cost
```

The first half is fixed by hardware and already at its floor. The second half
is where most "noticeable" latency actually lives, and it is entirely outside
this driver's control -- but it is where the fix usually is.

## Stage 1: pad to MIDI event (this driver)

```
mk3d --diagnose
```

covers this stage: device permissions, the real-time limits, whether
`SCHED_FIFO` was actually granted. Read its output before assuming a code
problem.

| stage                      | cost       | tunable?                       |
| --------------------------- | ---------- | ------------------------------ |
| pad strike to HID report   | up to 1 ms | no -- `bInterval = 1`, set by the firmware |
| parse, map, dispatch       | a few µs   | no -- already lock-free, alloc-free |
| ALSA sequencer to the host  | µs         | no                              |

Confirm the 1 ms floor and the real-time grant yourself:

```sh
lsusb -v -d 17cc:1600 | grep -A1 'bEndpointAddress.*0x83'   # input EP, look for bInterval 1
mk3d 2>&1 | grep '\[rt\]'                                    # "running SCHED_FIFO prio 80", or a fallback line
```

If the fallback line appears (`staying on SCHED_OTHER`), you are missing
`rtprio` headroom:

```sh
sudo usermod -aG audio "$USER"    # then log out and back in
```

Without it the driver still runs; a busy desktop can add scheduler jitter on
top of the 1 ms floor, but this is rarely the dominant cost -- check stage 3
first.

**The screen is not a suspect.** LED and display writes go out a separate USB
interface, over a separate file descriptor, on a separate thread, talking to
the core thread only through `try_lock` snapshots that never block. A busy
screen cannot delay a note. If you want to verify this on your own build
rather than take it on faith: `src/rt.rs` for the scheduling, `mk3d.rs`'s
`core_session` / `surface_session` split for the threading, `src/hid.rs` for
the fd cloning.

## Stage 2: which MIDI port are you actually on

Before chasing timing, rule out a routing mistake that *looks* like latency.
The MK3 exposes two independent MIDI paths:

```sh
aconnect -l
```

```
client 28: 'Maschine MK3' [type=kernel,card=3]        <- the hardware's own USB-MIDI class interface
    0 'Maschine MK3 MIDI 1'
client 128: 'Maschine MK3' [type=user]                 <- this driver's virtual port
    0 'Controller Out'
    1 'Controller In'
```

Client 28 is the DIN jacks / class-compliant interface, handled entirely by
the kernel, with no relation to this driver's mapping. If your DAW is
subscribed to client 28 instead of client 128, you are hearing raw device
MIDI, and every setting in `config.toml` is doing nothing. `mk3d --diagnose`
names the port you should be on; `mk3d --test-midi` confirms what actually
arrives.

## Stage 3: the audio buffer (usually the real cost)

This is outside the driver, and it is usually bigger than everything above it
combined. If you're on PipeWire, check the quantum:

```sh
pw-metadata -n settings 0 | grep quantum
```

```
update: id:0 key:'clock.quantum'      value:'1024' type:''   # ~21 ms at 48 kHz
update: id:0 key:'clock.min-quantum'  value:'32'   type:''   # ~0.7 ms
update: id:0 key:'clock.max-quantum'  value:'2048' type:''
```

1024 samples at 48 kHz is about 21 ms -- roughly twenty times the driver's own
budget. Lower it for the session:

```sh
pw-metadata -n settings 0 clock.force-quantum 128    # ~2.7 ms
```

```sh
pw-metadata -n settings 0 clock.force-quantum 0      # back to PipeWire's own default
```

Trade-off: a lower quantum means more interrupts per second on *every*
running audio client, not just this driver's. On a loaded or weak CPU that
shows up as crackle or xruns rather than as latency, so come down in steps --
try 256 before 128 -- and settle at the lowest value that runs clean for the
session you're actually doing.

`clock.force-quantum` reverts on PipeWire restart or reboot; it is a session
knob, not a permanent setting. To persist a value, drop a config fragment in
`~/.config/pipewire/pipewire.conf.d/`, per PipeWire's own configuration docs
-- deliberately not something this driver's install script touches, since it
affects every other audio application on the machine, not just this one.

If you're on plain ALSA rather than PipeWire, the equivalent knob is the
synth/DAW's own buffer size setting -- there is no system-wide quantum to
tune.

## Summary

If it still feels laggy after `force-quantum` is down and `--test-midi`
confirms the routing is right, the remaining cost is in the synth itself
(plugin latency, its own internal buffering) -- outside anything on this
list.
