local M = {}

function M.vision_bool(ctx, prompt, image_base64)
  local result = ctx:invoke("ollama:vision", {
    prompt = prompt,
    image_base64 = image_base64,
  })

  return result.boolean == true
end

function M.short_chat(ctx, prompt)
  local result = ctx:invoke("ollama:chat", {
    messages = {
      {
        role = "system",
        content = "Be concise.",
      },
      {
        role = "user",
        content = prompt,
      },
    },
  })

  if result.message then
    return result.message.content
  end

  return nil
end

return M
