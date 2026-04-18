return {
  id = "wakeup",
  name = "Wakeup",
  description = "Turn on bedroom lamps, wait, then the TV",
  mode = "single",
  execute = function(ctx)
    ctx:command_group("bedroom_lamps", { capability = "power", action = "on" })
    ctx:sleep(10)
    ctx:command("roku_tv:tv", { capability = "power", action = "on" })
  end
}