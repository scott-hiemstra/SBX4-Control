-- Gives the Sound Blaster X4 a 2.1 (speakers + subwoofer) output using the
-- profile set in ../alsa-card-profile/sound-blaster-x4.conf.
-- For WirePlumber 0.4 (Ubuntu 24.04, Debian 12); 0.5+ reads 51-sound-blaster-x4.conf.
-- Copy to ~/.config/wireplumber/main.lua.d/
table.insert(alsa_monitor.rules, {
  matches = {
    {
      { "device.name", "matches", "alsa_card.usb-Creative_Technology_Ltd_Sound_Blaster_X4*" },
    },
  },
  apply_properties = {
    ["api.alsa.use-acp"] = true,
    ["device.profile-set"] = "sound-blaster-x4.conf",
    ["device.profile"] = "sound-blaster-x4-21",
  },
})
