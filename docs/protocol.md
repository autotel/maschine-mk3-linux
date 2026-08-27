# Maschine MK3 USB protocol

Everything here was read off the device itself: `lsusb -v -d 17cc:1600`, the HID
report descriptor at `/sys/class/hidraw/hidrawN/device/report_descriptor`, and
`HIDIOCGFEATURE` on each feature report the descriptor declares. Nothing was
disassembled and nothing is guessed unless it says so.

`17cc:1600`, `bcdDevice 1.45`.

## Interfaces

| # | Class / subclass | iInterface | Endpoints | Kernel driver |
|---|---|---|---|---|
| 0 | Audio, Control, protocol 32 (UAC2) | Maschine MK3 | — | `snd-usb-audio` |
| 1 | Audio, Streaming | — | `0x01` OUT iso, 208 B, 125 µs | `snd-usb-audio` |
| 2 | Audio, Streaming | — | `0x81` IN iso, 104 B, 125 µs | `snd-usb-audio` |
| 3 | Audio, MIDI Streaming | Maschine MK3 MIDI | `0x82` IN, `0x02` OUT, bulk 512 B | `snd-usb-audio` |
| 4 | HID | Maschine MK3 HID | `0x83` IN, `0x03` OUT, interrupt 64 B | `usbhid` |
| 5 | Vendor `0xff`, subclass `0xbd` | Maschine MK3 BD | `0x04` OUT, bulk 512 B | **none** |
| 6 | Application, DFU | Maschine MK3 DFU | — | none |

Interface 5 being unclaimed is what makes the screens reachable without
unbinding anything.

### Audio

Playback: 4 channels, `S32_LE`, 24 significant bits, 44100 / 48000 / 88200 /
96000 Hz, asynchronous with implicit feedback from the capture endpoint.
Capture: 2 channels, same format and rates.

`bmChannelConfig` is 0 on both terminals, which is why ALSA guesses a surround
layout. The four outputs are Main L/R and Headphone L/R; the two inputs are the
line/mic pair.

### DIN MIDI

Interface 3 is ordinary USB MIDI class. It carries the rear jacks only, not the
control surface, and appears as its own ALSA client.

## HID (interface 4)

### Input report `0x01` — buttons, knobs, encoder, analog

41 payload bytes after the report id.

| offset | size | usage | range | contents |
|---|---|---|---|---|
| 0 | 10 | `0x02` | 0-1 | 80 button bits, byte 0 bit 0 first |
| 10 | 1 | `0x03` | 0-15 | two 4-bit counters; the low nibble is the 4-D encoder |
| 11 | 16 | `0x06` | 0-999 | 8 knobs, `u16` little-endian |
| 27 | 14 | `0x44`, `0x09`, `0x04` | see below | seven analog fields |

The descriptor splits those last fourteen bytes into a 0..65535 group of three
and two 0..4095 groups, but the declared ranges do not match what the hardware
sends. Read as one contiguous block of seven `u16` and measured on a real unit
while sliding a finger along the touch strip:

| index | offset | observed | what |
|---|---|---|---|
| 0 | 27 | climbs by ~16 every report, never resets | a free-running counter |
| 1 | 29 | 0-1024, drops to 0 on release | **the touch strip** |
| 2 | 31 | idle | unknown |
| 3 | 33 | idle | unknown |
| 4-6 | 35-39 | idle | the pedal jack |

Field 0 is a trap: it advances on every report, so a driver that treats it as a
control emits a continuous stream of CCs. The strip's full scale is 1024, not
the 4095 the descriptor implies.

The encoder nibble wraps, so a step is the shortest signed path between
readings: 15 → 0 is +1, not −15.

The 80-bit button field's assignment to physical buttons is **not** in the
descriptor. `mk3-learn buttons` discovers it by pressing. Confirmed so far:

| bit | byte.bit | button |
|---|---|---|
| 45 | 5.5 | Play |
| 56 | 7.0 | Channel / MIDI |

Note that reports keep flowing while a discovery tool waits for input, and the
hidraw buffer drops the oldest when it fills -- so a tool that compares against
a stale snapshot will silently miss presses. Re-read to the newest state before
waiting for each edge.

### Input report `0x02` — pads

63 payload bytes: 21 three-byte event slots, zero-padded. Decoding stops at the
first all-zero slot after the first.

```
byte 0    pad index, 0-15, numbered in reading order:
          0 is the TOP-left pad, 15 the bottom-right
byte 1    high nibble: event type
          low nibble: value bits 11-8
byte 2    value bits 7-0
```

| nibble | event |
|---|---|
| `0x00` | press on — a finger resting, no strike |
| `0x10` | note on — struck, value is velocity |
| `0x20` | press off |
| `0x30` | note off |
| `0x40` | aftertouch — value is pressure |

Values are 12-bit. The device sends aftertouch continuously while a pad is held,
so the pads are genuinely pressure sensitive, not just velocity sensitive.

**Velocity and pressure use very different parts of the range.** Both measured
on a real unit:

*Strike velocity* stays low. Across all sixteen pads a hard hit lands around
1950 and ordinary playing sits between 200 and 1400; nothing observed came near
4095. Scaling against the full 12-bit range makes every hit feel weak, so
`pads.velocity_max` defaults to 2000 and clips above that.

*Pressure* uses the whole range, but not evenly. Leaning on one pad from
nothing to as hard as is comfortable:

```
29  40  28  42  35  62  70  87  93  124  146  183  255  323  454
699  1007  1329  1566  2003  2769  3367  3662  3860  3982  4043  4094
```

Two thirds of the readings are below 500. The curve is steep at the top and
almost flat through the range a player actually lives in, so linear scaling
leaves aftertouch near silent for most of the gesture. Hence a separate
`pads.aftertouch_curve`, defaulting to `soft`.

Three ordering quirks worth knowing:

* Aftertouch arrives **before** `PressOn`, and can arrive before `NoteOn` for
  the same pad. A driver that forwards key pressure blindly sends it for notes
  the host was never told about.
* A trailing `Aftertouch @ 0` follows the note off, in either order.
* A slow press produces `PressOn` and pressure but **no** `NoteOn` at all --
  the device only calls it a note when the pad is struck.

### Output report `0x80` — LED bank 0

62 bytes, each 0-127.

### Output report `0x81` — LED bank 1

41 bytes, each 0-127.

103 slots in total, addressed by the driver as one flat array.

| slots | count | what |
|---|---|---|
| 0-61 | 62 | buttons |
| 62-86 | 25 | the touch strip |
| 87-102 | 16 | the pad grid |

Monochrome LEDs read their byte as brightness across the full 0-127 range.
Colour LEDs read it as `(palette_index << 2) | level`, where `level` is 0-3 and
0 means off regardless of the palette index. Not every button is monochrome —
Sampling, for one, is a colour LED.

The touch strip decodes colour a third way: the byte that renders green on a
pad renders violet on the strip, and neither palette holds violet at that
index. Its encoding is still unknown.

Full details and the confirmed per-button slots are in
[`hardware-map.md`](hardware-map.md).

### Feature reports

All confirmed by reading them back off the device.

| id | size | direction | contents |
|---|---|---|---|
| `0xd0` | 32 | read/write | device configuration; ends with three `(0, mid, 4095)` triplets that match the three 12-bit analog inputs, so it holds their calibration |
| `0xd8` | 32 | read only | hardware identity |
| `0xd9` | 32 | read only | serial number, ASCII, NUL-padded |
| `0xda` | 32 | read only | 16 × `u16` — per-pad sensor baseline |
| `0xdb` | 32 | read only | 16 × `u16` — per-pad sensor range |
| `0xf0` | 11 | read/write | 8 bytes then 3; sensitivity, read as `03` ×8 and `08` ×3 |
| `0xf2` | 6 | read/write | two triplets, 0-100 |
| `0xf8` | 10 | read/write | display 0 |
| `0xf9` | 10 | read/write | display 1 |
| `0xfe` | 208 | read/write | colour palette A |
| `0xff` | 208 | read/write | colour palette B |
| `0xf3`, `0xf4` | 1, 32 | write | output reports, purpose not established |

Sample read from a real unit:

```
0xda  [1526, 1459, 1481, 1406, 1482, 1433, 1341, 1316,
       1614, 1462, 1455, 1503, 1290, 1359, 1368, 1364]
0xdb  [3469, 3316, 3367, 3196, 3370, 3257, 3048, 2992,
       3670, 3324, 3308, 3418, 2933, 3089, 3111, 3100]
```

Baselines around 1300-1600 and ranges around 3000-3700, per pad. These are the
factory calibration and explain why raw pad values differ between pads.

#### Display feature reports `0xf8` / `0xf9`

```
offset 0   u16 LE   width          0x01e0 = 480
offset 2   u16 LE   height         0x0110 = 272
offset 4   u8       bits per pixel 0x10   = 16
offset 5   u8       0x60
offset 6   u8       0x01
offset 7   u8       brightness, 0-100
offset 8   u8       contrast, 0-100
offset 9   u8       0
```

Writing the report back with bytes 7 and 8 changed sets the backlight. The
geometry fields must be echoed unchanged.

#### Colour palettes `0xfe` / `0xff`

208 bytes of 7-bit RGB triplets, four consecutive triplets per palette entry —
one per brightness step. The first four entries read as black, then the ramps
begin:

```
entry 0   (0,0,0)     (0,0,0)     (0,0,0)     (0,0,0)
entry 1   (42,0,0)    (62,0,0)    (127,0,0)   (127,30,10)     red
entry 2   (42,8,0)    (62,16,0)   (127,25,0)  (127,45,5)      orange
...
```

The fourth step of each ramp adds white, which is how the device gets a
"brighter than full" look out of a 7-bit channel. Both palettes are writable, so
the colour names are not fixed in hardware.

## Displays (interface 5, bulk endpoint `0x04` OUT)

Two 480 × 272 panels, RGB565, index 0 is the left screen.

One frame is a single bulk transfer:

```
84 00 SS 60 00 00 00 00     header; SS = display index
xxxx yyyy wwww hhhh         destination rect, big-endian u16
02 00 00 00                 command: pixel data follows
0000 llll                   payload length in 32-bit words = w*h/2
<w*h pixels, RGB565, big-endian>
02 00 00 00                 command: end of pixel data
03 00 00 00                 command: blit
40 00 00 00                 command: end of frame
```

Because the length is counted in 32-bit words, `w * h` must be even. Partial
rectangles work, which is what makes a 30 KB dirty-row update possible instead
of a 261 KB full frame.

A full 480 × 272 frame is 261,120 bytes of pixel data. Measured on this unit,
pushing two of them takes about 75 ms, so roughly 7 MB/s — a full-frame refresh
rate of about 13 fps per screen, and far more than that for partial updates.

This command format is the same one the Komplete Kontrol MK2 uses; it was first
published in [qKontrol](https://github.com/GoaSkin/qKontrol). What differs on
the MK3 is the interface number (5, not 3) and the endpoint (`0x04`, not
`0x03`).

## Things still unknown

* The three `u16` fields at offset 27 of report `0x01` (`usage 0x44`,
  0-65535). They may be unused padding shared with another product in the
  family.
* Which of the four 12-bit analog inputs is the touch strip and which are the
  pedal jack's tip and ring. `mk3-learn watch` answers this on a given unit in
  seconds.
* Output reports `0xf3` (1 byte) and `0xf4` (1 + 31 bytes).
* Which of the 62 button slots are monochrome and which are colour. Slot 9
  (Sampling) is known to be colour; `mk3-learn probe rgb` shows the rest at a
  glance.
* How the touch strip decodes its colour byte. It is neither of the two
  palettes and it is not plain brightness.
* Button slots 16-61.
