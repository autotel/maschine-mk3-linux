# Hardware map

What is where on a Maschine MK3, read off the device rather than guessed. All
of it was found with `mk3-learn`: `buttons` for the bit indices, `leds` and
`probe` for the LED slots.

## Button bits

Input report `0x01` carries an 80-bit field. The descriptor does not say which
bit is which button; this table does. 66 of the 80 are accounted for.

| byte | bit 0 | bit 1 | bit 2 | bit 3 | bit 4 | bit 5 | bit 6 | bit 7 |
|---|---|---|---|---|---|---|---|---|
| **0** (0-7) | jogwheel-push | — | jogwheel-tilt-up | jogwheel-tilt-right | jogwheel-tilt-down | jogwheel-tilt-left | shift | screen-8 |
| **1** (8-15) | a | b | c | d | e | f | g | h |
| **2** (16-23) | notes | volume | swing | tempo | note-repeat | lock | — | — |
| **3** (24-31) | pad-mode | keyboard | chords | step | fixed-vel | scene | pattern | events |
| **4** (32-39) | — | variation | duplicate | select | solo | mute | pitch | mod |
| **5** (40-47) | perform | restart | erase | tap | follow | play | rec | stop |
| **6** (48-55) | macro | settings | arrow-right | sampling | mixer | plug-in | — | — |
| **7** (56-63) | channel-midi | arranger | browser | arrow-left | file | auto | — | — |
| **8** (64-71) | screen-1 | screen-2 | screen-3 | screen-4 | screen-5 | screen-6 | screen-7 | jogwheel-touched |
| **9** (72-79) | knob-8-touch | knob-7-touch | knob-6-touch | knob-5-touch | knob-4-touch | knob-3-touch | knob-2-touch | knob-1-touch |

Notes on the shape of it:

* **The knob touch sensors run backwards**: bit 72 is knob 8, bit 79 is knob 1.
* **Bit 71 is the encoder's touch sensor**, sitting immediately below the eight
  knob sensors — nine capacitive sensors in a descending run from bit 79.
* **Screen button 8 is at bit 7**, in byte 0, not with the other seven in byte 8.

### Unused bits

`1 22 23 32 54 55 62 63`. Every labelled control on the panel is accounted for,
so these are firmware padding, and their positions say so: 22/23, 54/55 and
62/63 are the top two bits of their bytes.

The encoder is the awkward one to map, because its touch sensor (bit 71) fires
a few milliseconds *before* the direction bit. A tool that reads one report
records the touch every time and the directions become unreachable. Both
`mk3-learn buttons` and `mk3-learn leds` collect everything that goes down over
a 350 ms window and prefer a bit that is not already spoken for; `ignore 71`
removes it from consideration outright.

## LED slots

Two output reports, 62 bytes and 41, addressed as one flat array of 103 slots.

| slots | count | what |
|---|---|---|
| 0-61 | 62 | buttons, all identified |
| 62-86 | 25 | touch strip |
| 87-102 | 16 | pad grid |

### Button LED slots

All 62 slots in output report `0x80`, complete. Ten buttons have no LED and
there are no slots left over, which confirms it: `jogwheel-push`,
`jogwheel-touched` and the eight `knob-N-touch` sensors.

| slot | button | | slot | button |
|---|---|---|---|---|
| 0 | channel-midi | | 31 | c *(colour)* |
| 1 | plug-in | | 32 | d *(colour)* |
| 2 | arranger | | 33 | e *(colour)* |
| 3 | mixer | | 34 | f *(colour)* |
| 4 | browser | | 35 | g *(colour)* |
| 5 | sampling *(colour)* | | 36 | h *(colour)* |
| 6 | arrow-left | | 37 | restart |
| 7 | arrow-right | | 38 | erase |
| 8 | file | | 39 | tap |
| 9 | settings | | 40 | follow |
| 10 | auto | | 41 | play *(colour)* |
| 11 | macro | | 42 | rec *(colour)* |
| 12 | screen-1 | | 43 | stop |
| 13 | screen-2 | | 44 | shift |
| 14 | screen-3 | | 45 | fixed-vel |
| 15 | screen-4 | | 46 | pad-mode |
| 16 | screen-5 | | 47 | keyboard |
| 17 | screen-6 | | 48 | chords |
| 18 | screen-7 | | 49 | step |
| 19 | screen-8 | | 50 | scene |
| 20 | volume | | 51 | pattern |
| 21 | swing | | 52 | events |
| 22 | note-repeat | | 53 | variation |
| 23 | tempo | | 54 | duplicate |
| 24 | lock | | 55 | select |
| 25 | pitch | | 56 | solo |
| 26 | mod | | 57 | mute |
| 27 | perform | | 58 | jogwheel-tilt-up *(colour)* |
| 28 | notes | | 59 | jogwheel-tilt-left *(colour)* |
| 29 | a *(colour)* | | 60 | jogwheel-tilt-right *(colour)* |
| 30 | b *(colour)* | | 61 | jogwheel-tilt-down *(colour)* |

Slots marked *(colour)* decode their byte as `(palette << 2) | level` rather
than as brightness.

### Pad numbering

The device numbers pads in **reading order**: HID pad 0 is the **top-left**
pad, the one silkscreened 13.

```
HID   0  1  2  3     silkscreen  13 14 15 16     top row
HID   4  5  6  7                  9 10 11 12
HID   8  9 10 11                  5  6  7  8
HID  12 13 14 15                  1  2  3  4     bottom row
```

`pads.notes` is indexed by HID number. The shipped default is transposed so the
bottom-left pad plays the lowest note.

### Three colour encodings

A **monochrome** LED reads its byte as brightness across 0-127.

A **colour** LED reads it as `(palette_index << 2) | level`, where `level` is
0-3 and level 0 is off whatever the palette says.

```sh
mk3-learn probe rgb     # 0x05: only colour LEDs light
mk3-learn probe mono    # 0x7c: only monochrome ones light
```

Not every button is monochrome — Sampling (slot 5) is a colour LED.

The **touch strip** decodes colour a third way: the byte that renders green on
a pad renders violet on the strip, and neither of the device's two palettes
(`0xfe`, `0xff`) holds violet at that index. So `touchstrip.led_value` is a raw
byte rather than a palette index, and the encoding is an open question in
[`protocol.md`](protocol.md).

## Analog fields

Seven `u16` at offsets 27-41 of report `0x01`:

| index | observed | what |
|---|---|---|
| 0 | climbs ~16 per report, never resets | a free-running counter — **not** a control |
| 1 | 0-1024, drops to 0 on release | the touch strip |
| 2, 3 | idle | unknown |
| 4-6 | idle | the pedal jack |

## Palettes

Feature reports `0xfe` and `0xff` each hold 208 bytes: four 7-bit RGB triplets
per entry, one per brightness step. Both are writable, so pad colours are not
fixed in hardware.

```sh
mk3-learn palette
```
