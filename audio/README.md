# Audio on the Maschine MK3

The MK3's sound card needs no driver work: interfaces 0-2 are USB Audio Class 2
compliant, so `snd-usb-audio` already binds them. What the files here fix is how
the rest of the stack presents that card.

## What the hardware actually offers

Read straight off the USB descriptors:

| | |
|---|---|
| Playback | 4 channels, 24-bit (`S32_LE`), 44.1 / 48 / 88.2 / 96 kHz |
| Capture | 2 channels, 24-bit, same rates |
| Packet interval | 125 µs (USB high-speed microframes) |
| Sync | implicit feedback from the capture endpoint |

The four outputs are **Main L/R** and **Headphone L/R**. The two inputs are the
line/mic input pair.

That is the whole of it — there is no hidden set of extra channels waiting for a
proprietary driver. The MK3 exposes exactly this over USB on every operating
system; the extra routing in NI's own software is done in the host, not in the
box.

## The two problems, and the fixes

**The channel map is a guess.** The device declares `bmChannelConfig = 0`, so
ALSA falls back to "Analog Surround 4.0" and labels the outputs `FL FR FC LFE`.
Anything a program sends to "centre" or "LFE" comes out of the headphones.
`51-maschine-mk3.lua` switches the card to the `pro-audio` profile, which drops
the surround pretence and gives four plain numbered ports.

**The quantum is desktop-sized.** PipeWire's stock 1024 frames is 21 ms at
48 kHz, which you can feel between striking a pad and hearing it.
`99-maschine-lowlatency.conf` takes it to 128 frames, or 2.7 ms.

## Install

```sh
mkdir -p ~/.config/wireplumber/main.lua.d ~/.config/pipewire/pipewire.conf.d
cp audio/51-maschine-mk3.lua      ~/.config/wireplumber/main.lua.d/
cp audio/99-maschine-lowlatency.conf ~/.config/pipewire/pipewire.conf.d/
systemctl --user restart pipewire pipewire-pulse wireplumber
```

WirePlumber 0.5 and later use `.conf` files rather than Lua. If
`wireplumber --version` reports 0.5 or newer, put this in
`~/.config/wireplumber/wireplumber.conf.d/51-maschine-mk3.conf` instead:

```
monitor.alsa.rules = [
  {
    matches = [ { device.name = "~alsa_card.usb-Native_Instruments_Maschine_MK3.*" } ]
    actions = { update-props = { api.alsa.use-acp = false, device.profile = "pro-audio" } }
  }
  {
    matches = [ { node.name = "~alsa_(output|input).usb-Native_Instruments_Maschine_MK3.*" } ]
    actions = { update-props = {
      audio.format = "S32LE"
      audio.rate = 48000
      api.alsa.period-size = 128
      api.alsa.headroom = 128
      session.suspend-timeout-seconds = 0
    } }
  }
]
```

## Checking it worked

```sh
pw-metadata -n settings | grep quantum     # should show 128
pw-top                                     # watch for xruns while playing
```

`pw-top`'s `ERR` column counts xruns. If it climbs during normal playing, raise
`default.clock.min-quantum` to 256 and try again.

## Latency you can expect

| stage | cost |
|---|---|
| pad strike to HID report | up to 1 ms (the endpoint's polling interval) |
| driver processing | a few µs |
| ALSA sequencer to the host | microseconds |
| host processing | one quantum, 2.7 ms at 128 frames |
| audio out | one quantum plus the USB packet interval |

Round trip lands in the region of 6-8 ms with a 128-frame quantum. The 1 ms HID
poll is a hardware property of the endpoint (`bInterval = 1` on a high-speed
interrupt endpoint) and is the one part of this that cannot be tuned away.

## External power

The MK3 runs fine bus-powered — the descriptor asks for 480 mA and reports
`Bus Powered`. The external supply matters when you are driving loud headphones
or a hot line input; it does not change what the audio interface offers.

## The MIDI jacks

The rear DIN in/out are a separate USB MIDI-class interface (interface 3), also
already handled by the kernel. They appear as their own ALSA client:

```
client 28: 'Maschine MK3' [type=kernel,card=3]
    0 'Maschine MK3 MIDI 1'
```

That client is the physical DIN sockets, not the control surface. The control
surface is the port `mk3d` creates.
