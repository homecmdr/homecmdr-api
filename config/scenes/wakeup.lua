return {
  id = "wakeup",
  name = "Wakeup",
  description = "Fade bedroom lamps from dim to 70% over 30s, then turn on the TV",
  mode = "single",
  execute = function(ctx)
    -- bring lamps on at minimum brightness so the transition starts from nearly off
    ctx:command_group("bedroom_lamps", { capability = "brightness", action = "set", value = 1 })

    -- ask the bulbs to fade to 70% over 30 seconds in hardware
    ctx:command_group("bedroom_lamps", {
      capability = "brightness",
      action = "set",
      value = 70,
      transition_secs = 30,
    })

    -- wait for the transition to finish, then turn on the TV
    ctx:sleep(30)
    ctx:command("roku_tv:tv", { capability = "power", action = "on" })
  end
}
