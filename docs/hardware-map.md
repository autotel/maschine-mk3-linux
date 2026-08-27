# Hardware map

What is where on a Maschine MK3, read off the device rather than guessed. All
of it was found with `mk3-learn`: `buttons` for the bit indices, `leds` and
`probe` for the LED slots.

## Button bits

Input report `0x01` carries an 80-bit field. The descriptor does not say which
bit is which button; this table does. 66 of the 80 are accounted for.

| byte | bit 0 | bit 1 | bit 2 | bit 3 | bit 4 | bit 5 | bit 6 | bit 7 |
|---|---|---|---|---|---|---|---|---|
| **0** (0-7) | — | — | — | — | — | — | Shift | Screen 8 |
| **1** (8-15) | Group A | B | C | D | E | F | G | H |
| **2** (16-23) | Notes | Volume | Swing | Tempo | — | Lock | — | — |
| **3** (24-31) | Pad Mode | Keyboard | Chords | Step | Fixed Vel | Scene | Pattern | Events |
| **4** (32-39) | — | Variation | Duplicate | Select | Solo | Mute | Pitch | Mod |
| **5** (40-47) | Perform | Restart | Erase | Tap | Follow | **Play** | Rec | Stop |
| **6** (48-55) | Macro | Settings | Arrow right | Sampling | Mixer | Plug-in | — | — |
| **7** (56-63) | **Channel/MIDI** | Arranger | Browser | Arrow left | File | Auto | — | — |
| **8** (64-71) | Screen 1 | 2 | 3 | 4 | 5 | 6 | 7 | Encoder touch |
| **9** (72-79) | Knob 8 touch | 7 | 6 | 5 | 4 | 3 | 2 | Knob 1 touch |

Notes on the shape of it:

* **The knob touch sensors run backwards**: bit 72 is knob 8, bit 79 is knob 1.
* **Bit 71 is the encoder's touch sensor**, sitting immediately below the eight
  knob sensors — nine capacitive sensors in a descending run from bit 79.
* **Screen button 8 is at bit 7**, in byte 0, not with the other seven in byte 8.

### Still unidentified

Bits `0 1 2 3 4 5 20 22 23 32 54 55 62 63`.

Six consecutive free bits at the bottom of byte 0 is a strong hint: the 4-D
encoder needs five (press plus four tilts) and Note Repeat is the one labelled
button not yet accounted for. Byte 2's gaps at 20, 22 and 23 sit next to Lock
and Notes, which is where Note Repeat lives on the panel.

The encoder is awkward to map because its touch sensor fires a few
milliseconds before the direction bit. `mk3-learn buttons` handles this by
accumulating everything that goes down over a 350 ms window and preferring a
bit that is not already named, so once `bit 71` has a name the directions come
through. `ignore 71` takes it out of consideration entirely.

## LED slots

Two output reports, 62 bytes and 41, addressed as one flat array of 103 slots.

| slots | count | what |
|---|---|---|
| 0-61 | 62 | buttons |
| 62-86 | 25 | touch strip |
| 87-102 | 16 | pad grid |

### Confirmed button LED slots

They come in fours, matching the panel's clusters:

| slot | button | | slot | button |
|---|---|---|---|---|
| 0 | Channel / MIDI | | 8 | File / Save |
| 1 | Plug-in / Instance | | 9 | Settings |
| 2 | Arranger | | 10 | Auto |
| 3 | Mixer | | 11 | Macro / Set |
| 4 | Browser / +Plug-in | | 12 | Screen button 1 |
| 5 | Sampling | | 13 | Screen button 2 |
| 6 | Arrow left | | 14 | Screen button 3 |
| 7 | Arrow right | | 15 | Screen button 4 |
| 16 | Screen button 5 | | 18 | Screen button 7 |
| 17 | Screen button 6 | | 19 | Screen button 8 |

Slots 20-61 are the remaining buttons. Step through them with
`mk3-learn leds 20 62`, which records each answer into the config as you go.

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
