# Presets

A preset is a whole set of settings under a name. Switching one replaces what
every control does at once, which is the difference between "my drum kit" and
"my mixer layout" being two files rather than an evening of re-editing.

They are ordinary config files. Sharing one means sending a file; reading one
means opening it in an editor. There is nothing else to learn.

## What ships

| | |
|---|---|
| `default` | Every control mapped. Pads on channel 10, knobs on 1, each button on its own CC on channel 16. |
| `drums` | General MIDI kit on the pads, kick and snare under the left hand. Transport on the buttons, knobs as a small mixer. Aftertouch off. |
| `keys` | Two chromatic octaves from C2, polyphonic aftertouch on the soft curve, knobs on the usual synth CCs, strip on modulation. |
| `mixer` | Eight channel strips. Knobs are level, group buttons are latching mutes, top rows of pads solo and arm. |
| `minimal` | Pads and knobs only. Every button listed but silent — a base to build your own on without having to look up any names. |

## Using them

```sh
mk3d --list-presets        # what is available, and which is loaded
mk3d --preset drums        # load one and run
mk3d --save-preset my-kit  # save the current config under a name
```

Or in `mk3-gui`, from the preset bar: pick one from the list, **Save as...** to
name the current settings, **Import...** to paste one someone sent you, and
**Folder** to open the directory they live in.

## Where they live

```
~/.config/maschine-mk3/presets/*.toml
```

The five above are compiled into the driver, so a fresh install has something
to choose from with nothing installed. **A user file of the same name shadows a
built-in one**, so you can adjust `drums` to taste without losing the original:
delete your copy and the shipped one comes back.

## Sharing

Send the file. On the other end, drop it in the presets directory, or paste it
into **Import...**.

An imported preset is checked against the device before anything is written, so
a file naming a control this hardware does not have is refused with a
suggestion rather than half-applied:

```
buttons.plya: no control called that; did you mean play?
```

## Switching is reversible

Loading a preset overwrites `config.toml`. The previous one is kept beside it
as `config.toml.prev`, so an unwanted switch is one copy away from being undone:

```sh
cp ~/.config/maschine-mk3/config.toml.prev ~/.config/maschine-mk3/config.toml
```

## Writing your own

Start from `minimal`, which lists every control bound to `"none"`, and fill in
the ones you want. Then give it a header so it can describe itself:

```toml
[preset]
name = "my-kit"
description = "Pads for the sampler, knobs on the filter."
author = "you"
```

`mk3d --save-preset` and the GUI's **Save as...** write that header for you.

A preset says nothing about *where* controls are wired -- that is the device
profile in `devices/`, and it is deliberately not part of a preset. It means a
preset written on one MK3 works on another, and that switching preset cannot
break the panel.

## Contributing one

Presets for particular hosts are welcome -- a Bitwig layout, an Ableton drum
rack, a Renoise mapping. Drop a file in this directory with a `[preset]` header
and a line in the table above. `cargo test` checks that everything here parses
and fits the device.
