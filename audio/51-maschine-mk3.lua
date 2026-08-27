-- WirePlumber 0.4 rule for the Maschine MK3's audio interface.
--
-- Two things are wrong with the defaults, both caused by the device's USB
-- descriptor declaring bmChannelConfig = 0:
--
--   * ALSA guesses "Analog Surround 4.0" and labels the four outputs
--     FL FR FC LFE. They are actually Main L/R and Headphone L/R, so anything
--     routed to "centre" or "LFE" lands in the headphones.
--   * The device gets a desktop-sized period, which is fine for playback and
--     useless for playing an instrument.
--
-- Install to ~/.config/wireplumber/main.lua.d/51-maschine-mk3.lua and restart
-- WirePlumber:  systemctl --user restart wireplumber

rule = {
  matches = {
    {
      { "device.name", "matches", "alsa_card.usb-Native_Instruments_Maschine_MK3*" },
    },
  },
  apply_properties = {
    -- Expose the raw 4-out/2-in interface instead of a guessed surround
    -- profile, and keep both directions available at once.
    ["api.alsa.use-acp"] = false,
    ["api.alsa.disable-mixer"] = true,
    ["api.alsa.soft-mixer"] = false,
    ["device.profile"] = "pro-audio",
  },
}

table.insert(alsa_monitor.rules, rule)

node_rule = {
  matches = {
    {
      { "node.name", "matches", "alsa_output.usb-Native_Instruments_Maschine_MK3*" },
    },
    {
      { "node.name", "matches", "alsa_input.usb-Native_Instruments_Maschine_MK3*" },
    },
  },
  apply_properties = {
    -- 24-bit over the wire, which is what the endpoint actually carries.
    ["audio.format"] = "S32LE",
    ["audio.rate"] = 48000,
    -- 128 frames at 48 kHz is 2.7 ms per period. The endpoint runs at a
    -- 125 us interval, so this is comfortably above what the hardware needs
    -- while staying inside the range a player can feel.
    ["api.alsa.period-size"] = 128,
    ["api.alsa.headroom"] = 128,
    -- Two periods: one being filled while the other drains.
    ["api.alsa.periods"] = 2,
    ["session.suspend-timeout-seconds"] = 0,
    ["node.pause-on-idle"] = false,
  },
}

table.insert(alsa_monitor.rules, node_rule)
