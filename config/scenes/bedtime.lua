return {
  id = "bedtime",
  name = "Bedtime",
  description = "Smoothly dim bedroom lamps to off over 10s, then turn off the TV",
  mode = "single",
  execute = function(ctx)
    -- ask the bulbs to fade to zero over 10 seconds in hardware
    ctx:command_group("bedroom_lamps", {
      capability = "brightness",
      action = "set",
      value = 0,
      transition_secs = 10,
    })

    -- wait for the transition to finish, then cut power
    ctx:sleep(10)
    ctx:command_group("bedroom_lamps", { capability = "power", action = "off" })

    ctx:sleep(5)
    ctx:command("roku_tv:tv", { capability = "power", action = "off" })
  end
}
