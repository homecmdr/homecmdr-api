return {
  id = "rain_reminder",
  name = "Rain Reminder",
  description = "Example device-state automation for rain detection.",
  trigger = {
    type = "device_state_change",
    device_id = "weather:outside",
    attribute = "rain",
    equals = true,
  },
  execute = function(ctx, event)
    local result = ctx:invoke("ollama:chat", {
      messages = {
        {
          role = "system",
          content = "Reply in one short sentence.",
        },
        {
          role = "user",
          content = "It's raining. Suggest a short reminder to check the clothesline.",
        },
      },
    })

    if result.message and result.message.content then
      -- Replace this with a real notifier device when available.
      ctx:command("roku_tv:tv", {
        capability = "power",
        action = "off",
      })
    end
  end,
}
