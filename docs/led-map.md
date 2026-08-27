# LED map

The MK3 takes two output reports, both plain arrays of bytes:

* report `0x80` — 62 bytes
* report `0x81` — 41 bytes

The driver addresses them as one flat space of 103 slots, so slot 0-61 is
report `0x80` and slot 62-102 is report `0x81`.

## Layout

| slots | count | what |
|---|---|---|
| 0-61 | 62 | buttons |
| 62-86 | 25 | the touch strip |
| 87-102 | 16 | the 4x4 pad grid |

That 41 = 25 + 16 split is what the second report is for. It was settled by
lighting four single slots in four different colours at once and reading off
which two landed on the pads:

```sh
# slot  62 red, slot 77 green  -> touch strip LEDs 1 and 16
# slot  87 blue, slot 102 white -> pads
```

Slot order ascends along the strip: slot 62 is one end, slot 86 the other.
Which end that is relative to the direction a finger travels is **not**
confirmed, so `touchstrip.led_reversed` exists to flip the meter.

Getting this backwards is easy and the symptom is unmistakable: tapping pads
lights the strip instead, and only sixteen of its twenty-five LEDs respond.

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
bottom-left pad plays the lowest note, which is what drum mapping conventions
expect.

## Confirmed button slots

| slot | button |
|---|---|
| 0 | first button above the displays (leftmost) |
| 1 | second button above the displays |
| 2 | third button above the displays |
| 3 | fourth button above the displays |
| 4 | File / Save |
| 5 | Settings |
| 6 | Auto |
| 7 | Macro / Set |
| 8 | Browser / +Plug-in |
| 9 | Sampling |
| 10 | left arrow, below Browser |
| 11 | right arrow |
| 12 | Channel / MIDI |
| 13 | Plug-in / Instance |
| 14 | Arranger |
| 15 | Mixer |
| 16-61 | not yet identified |

## Confirmed button bits

Separate from LED slots: this is the button's index in the 80-bit field of
input report `0x01`.

| bit | byte.bit | button |
|---|---|---|
| 45 | 5.5 | Play |
| 56 | 7.0 | Channel / MIDI |

Step through the rest with:

```sh
mk3-learn leds 16 62
```

## Two kinds of LED, two encodings

A **monochrome** LED reads its byte as brightness across 0-127.

A **colour** LED reads it as `(palette_index << 2) | level`, where `level` is
0-3 and level 0 is off whatever the palette index says.

The two are told apart without knowing which is which:

```sh
mk3-learn probe rgb     # 0x05 = palette 1, level 1: only colour LEDs light
mk3-learn probe mono    # 0x7c = palette 31, level 0: only mono LEDs light
```

Not every button is monochrome. Slot 9 (Sampling) is a colour LED, which is why
it comes up blue when the driver writes a palette-encoded value to it.

**The touch strip is a third case.** It decodes colour differently again: the
byte that renders green on a pad renders violet on the strip. Neither of the
device's two palettes (`0xfe`, `0xff`) explains it — both hold green at that
index. So `touchstrip.led_value` is a raw byte rather than a palette index, and
the encoding is left as an open question in
[`protocol.md`](protocol.md).

## Palettes

Feature reports `0xfe` and `0xff` each hold 208 bytes: four 7-bit RGB triplets
per palette entry, one per brightness step. Both are writable, so pad colours
are not fixed in hardware.

```sh
mk3-learn palette      # print both, as RGB
```

The fourth step of each ramp mixes in white, which is how the device gets a
"brighter than full" look out of a 7-bit channel.
