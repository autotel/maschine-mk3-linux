# Porting to another Native Instruments controller

The driver has no Maschine MK3 in it. What it has is a *shape*: an HID input
report with a button bitfield, some knobs and some analog fields; a second
report carrying pad events; output reports full of LED bytes; and, on the
devices that have screens, a vendor bulk endpoint that takes RGB565.

Every NI controller in this family fits that shape. They differ in offsets and
counts, which is what a device profile is for. Supporting a new one should mean
writing `devices/your-device.toml`, not editing the driver.

## What you need from the hardware

Everything below is readable from the device itself. Nothing needs a
disassembler.

```sh
lsusb -v -d 17cc:                        # interfaces and endpoints
cat /sys/class/hidraw/hidrawN/device/report_descriptor | xxd
```

The report descriptor gives you the report ids, the size and count of every
field, and their order. It does **not** tell you which bit is which button --
nothing does. That part is discovered by pressing.

## Filling in the profile

Start from `devices/maschine-mk3.toml` and change what differs.

```toml
[device]
name = "Your Device"
vendor = 0x17cc
product = 0x1234

[report]
controls = 0x01          # input report with buttons, knobs, analog
pads = 0x02              # input report with pad events, if any
led_banks = [[0x80, 62], [0x81, 41]]   # output reports: [id, bytes]

[layout]
buttons_offset = 0       # byte offsets inside the controls report
button_bits = 80
knobs_offset = 11
knobs = 8
knob_max = 999           # value the knobs wrap at
...
```

Then one table per control:

```toml
[control."play"]
kind = "button"          # button | pad | knob | encoder | strip | screen
label = "PLAY"
bit = 45                 # index in the button bitfield
led = 41                 # flat LED slot; omit if it has no light
led_colour = 17          # omit for a monochrome LED
group = "transport"
x = 12.1                 # position on the panel map, in grid units
y = 18.9
w = 3.4
h = 1.6
```

The driver validates the result on load: duplicate bits, duplicate LED slots,
LEDs inside a block it paints wholesale, blocks that overlap, and analog fields
that do not exist are all refused with a message naming the control.

## Discovering the parts nothing documents

```sh
mk3-learn watch raw      # every field, decoded, plus a hexdump on any change
mk3-learn buttons        # press each button; records its bit
mk3-learn leds 0 62      # lights a slot, you press what lit; records the slot
mk3-learn colours        # finds which LEDs decode colour rather than brightness
mk3-learn find-pads      # locates the pad LED block
mk3-learn probe A B      # light an arbitrary slot range
mk3-learn info           # the device's own feature reports
```

All of them write into the profile, not into anyone's settings.

Three things caught us out on the MK3 and are worth checking on any sibling:

* **Which analog field is which.** One of the MK3's seven is a free-running
  counter that advances on every report. Treating it as a control emits a CC
  forever. `watch` makes it obvious: it moves when nothing is being touched.
* **Touch sensors fire before the thing they belong to.** The MK3's encoder
  reports its capacitive sensor a few milliseconds before the direction being
  tilted, so a tool that reads one report records the sensor every time. The
  learn tools collect over a 350 ms window for this reason.
* **LED banks are mixed.** A monochrome LED reads its byte as brightness; a
  colour one reads it as `(palette << 2) | level`, where level 0 is off
  whatever the palette says. Writing a modest brightness to a colour LED picks
  a palette entry instead, and it comes up the wrong colour.

## What is still Maschine-specific

Honestly: some things.

* `src/hid.rs` decodes the pad event triplet and the knob layout at fixed
  offsets from the profile, but assumes the *shape* -- a 3-byte pad triplet
  with the event type in a nibble. Another family would need a variant.
* `src/display/` speaks the NI bulk display protocol. That part is shared with
  the Komplete Kontrol MK2, but the interface and endpoint numbers differ per
  device and are currently constants.
* `src/ui.rs` lays out a screen 480x272.

None of it is deep. The profile carries everything that is genuinely data;
what is left is where a second device would tell us what deserves to become
data too.

## Contributing a profile

A profile plus the output of `mk3-learn info` is enough for someone else to
use your device. Drop it in `devices/`.
