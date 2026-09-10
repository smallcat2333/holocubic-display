-- Create a native 320x240 quota scene; no image frames or SD writes are needed.
local function create_quota_scene(parent, remaining, reset_seconds, demo)
  local MAIN = LV_PART_MAIN
  local CYAN = 0x35E7FF
  local MUTED = 0x619AA8
  local scene = {
    remaining = remaining,
    reset_seconds = reset_seconds,
    reset_initial = reset_seconds,
    demo = demo,
    frames = 0,
    counter = 0,
    updates = 0,
    last_second = tmr.time(),
  }

  local container = lv_obj_create(parent)
  lv_obj_set_pos(container, 0, 0)
  lv_obj_set_size(container, 320, 240)
  lv_obj_set_style_bg_color(container, 0x000000, MAIN)
  lv_obj_set_style_bg_opa(container, 255, MAIN)
  lv_obj_set_style_border_width(container, 0, MAIN)
  lv_obj_set_style_pad_all(container, 0, MAIN)
  lv_obj_set_style_radius(container, 0, MAIN)
  lv_obj_clear_flag(container, LV_OBJ_FLAG_SCROLLABLE)
  local task_image = lv_img_create(container)
  lv_obj_set_pos(task_image, 0, 0)
  lv_obj_add_flag(task_image, LV_OBJ_FLAG_HIDDEN)

  -- Create a fixed-size colored rectangle in scene coordinates and return its ID.
  local function rectangle(x, y, width, height, color, radius)
    local obj = lv_obj_create(container)
    lv_obj_set_pos(obj, x, y)
    lv_obj_set_size(obj, width, height)
    lv_obj_set_style_bg_color(obj, color, MAIN)
    lv_obj_set_style_bg_opa(obj, 255, MAIN)
    lv_obj_set_style_border_width(obj, 0, MAIN)
    lv_obj_set_style_pad_all(obj, 0, MAIN)
    lv_obj_set_style_radius(obj, radius, MAIN)
    lv_obj_clear_flag(obj, LV_OBJ_FLAG_SCROLLABLE)
    return obj
  end

  -- Create an ASCII label using a font confirmed on firmware 1.002.
  local function label(text, x, y, width, height, font, color)
    local obj = lv_label_create(container)
    lv_obj_set_pos(obj, x, y)
    lv_obj_set_size(obj, width, height)
    lv_obj_set_style_text_font(obj, font, MAIN)
    lv_obj_set_style_text_color(obj, color, MAIN)
    lv_label_set_text(obj, text)
    return obj
  end

  -- Draw a static glass outline from local points; only sand objects will animate.
  local function outline(points)
    local obj = lv_line_create(container)
    lv_obj_set_pos(obj, 0, 0)
    lv_line_set_points(obj, points)
    lv_obj_set_style_line_width(obj, 2, MAIN)
    lv_obj_set_style_line_color(obj, 0x76D5E6, MAIN)
  end

  local title_label = label("CODEX", 16, 10, 105, 26, LV_FONT_MONTSERRAT_20, 0xE1F8FF)
  local subtitle_label = label("QUOTA", 113, 16, 76, 20, LV_FONT_MONTSERRAT_12, MUTED)
  rectangle(241, 12, 63, 22, 0x153039, 5)
  local badge = label("DEMO", 249, 16, 54, 16, LV_FONT_MONTSERRAT_12, 0xFFC276)
  rectangle(16, 43, 288, 1, 0x19343D, 0)
  rectangle(32, 56, 96, 5, 0x73D8E8, 2)
  rectangle(32, 201, 96, 5, 0x73D8E8, 2)
  outline({{x=36,y=64},{x=76,y=126},{x=76,y=134},{x=36,y=196}})
  outline({{x=124,y=64},{x=84,y=126},{x=84,y=134},{x=124,y=196}})

  local upper_rows, lower_rows = {}, {}
  for index = 1, 30 do
    local y = (index - 1) * 2
    local upper_half = math.max(1, math.floor(39 * (1 - (y + 1) / 60)))
    local lower_half = math.max(1, math.floor(39 * (y + 1) / 60))
    upper_rows[index] = rectangle(80 - upper_half, 66 + y, upper_half * 2, 2, CYAN, 0)
    lower_rows[index] = rectangle(80 - lower_half, 134 + y, lower_half * 2, 2, 0x287D90, 0)
  end

  local grains = {}
  for index = 1, 7 do
    grains[index] = rectangle(79, 130, 2, 3, 0xABF5FF, 0)
  end

  label("REMAINING", 162, 59, 142, 18, LV_FONT_MONTSERRAT_12, MUTED)
  local percent_label = label("72%", 160, 79, 144, 35, LV_FONT_MONTSERRAT_28, CYAN)
  rectangle(162, 119, 142, 4, 0x102B34, 1)
  local progress = rectangle(162, 119, 102, 4, CYAN, 1)
  local time_caption = label("RESET IN", 162, 139, 142, 17, LV_FONT_MONTSERRAT_12, MUTED)
  local clock_label = label("01:00:00", 162, 161, 142, 27, LV_FONT_MONTSERRAT_20, 0xE1F8FF)
  local count_label = label("COUNT 0000", 162, 191, 142, 18, LV_FONT_MONTSERRAT_12, MUTED)
  rectangle(16, 216, 288, 1, 0x19343D, 0)
  local footer = label("SIMULATED DATA / USB / LOCAL", 30, 225, 288, 14, LV_FONT_MONTSERRAT_10, MUTED)

  -- Toggle a native object without reallocating any pixel buffers or widgets.
  local function visible(obj, enabled)
    if enabled then
      lv_obj_clear_flag(obj, LV_OBJ_FLAG_HIDDEN)
    else
      lv_obj_add_flag(obj, LV_OBJ_FLAG_HIDDEN)
    end
  end

  -- Update sand area and labels once per value change; triangle area matches quota.
  local function update_values()
    local root_fraction = math.sqrt(scene.remaining / 100)
    local upper_surface = 60 * (1 - root_fraction)
    local lower_surface = 60 * root_fraction
    local accent = scene.task and scene.accent or (scene.remaining <= 20 and 0xFFBE67 or CYAN)
    local lower_color = scene.remaining <= 20 and 0x80512B or 0x287D90
    scene.landing_y = 134 + lower_surface
    for index = 1, 30 do
      local row_midpoint = (index - 1) * 2 + 1
      visible(upper_rows[index], scene.remaining > 0 and row_midpoint >= upper_surface)
      visible(lower_rows[index], scene.remaining < 100 and row_midpoint >= lower_surface)
      lv_obj_set_style_bg_color(upper_rows[index], accent, MAIN)
      lv_obj_set_style_bg_color(lower_rows[index], lower_color, MAIN)
    end
    for _, grain in ipairs(grains) do
      visible(grain, scene.remaining > 0 and (not scene.task or scene.reset_seconds > 0))
    end
    local bp = scene.task and scene.bp or math.floor(scene.remaining * 100 + 0.5)
    lv_label_set_text(percent_label, string.format("%d.%02d%%", math.floor(bp / 100), bp % 100))
    lv_obj_set_style_text_color(percent_label, accent, MAIN)
    lv_obj_set_style_bg_color(progress, accent, MAIN)
    lv_obj_set_size(progress, math.max(1, math.floor(142 * scene.remaining / 100)), 4)
    visible(progress, scene.remaining > 0)
    local state_text = scene.task and (scene.paused and "PAUSE" or (scene.reset_seconds == 0 and "DONE" or "RUN")) or (scene.demo and "DEMO" or "USB")
    lv_label_set_text(badge, state_text)
    lv_label_set_text(footer, scene.demo and "SIMULATED DATA / USB / LOCAL" or "PYTHON VALUES / USB / LOCAL")
  end

  -- Format bounded relative seconds, avoiding Unix timestamps in float32 JSON.
  local function update_clock()
    local seconds = scene.reset_seconds
    lv_label_set_text(clock_label, string.format("%02d:%02d:%02d",
      math.floor(seconds / 3600), math.floor(seconds / 60) % 60, seconds % 60))
    lv_label_set_text(count_label, string.format(scene.task and "ELAPSED %04d" or "COUNT %04d", scene.counter))
  end

  -- Apply one validated Python state message; non-demo quota never drains by time.
  function scene.update(new_remaining, new_reset_seconds, new_demo)
    scene.remaining = new_remaining
    scene.reset_seconds = new_reset_seconds
    scene.reset_initial = new_reset_seconds
    scene.demo = new_demo
    scene.last_second = tmr.time()
    scene.updates = scene.updates + 1
    update_values()
    update_clock()
  end

  -- Configure a timed/manual task after validated USB input and one label upload.
  function scene.configure_task(seconds, bp, timed, accent, looping)
    scene.task = true
    scene.bp = bp
    scene.initial_bp = bp
    scene.duration = seconds
    scene.timed = timed
    scene.looping = looping
    scene.accent = accent
    scene.paused = false
    scene.counter = 0
    scene.last_second = tmr.time()
    local jpeg = assert(file.getcontents("/sd/apps/usb_display/task_labels.jpg"), "missing task labels")
    scene.labels_jpeg = jpeg
    scene.labels_crc = string.format("%08x", zlib.crc32(jpeg, 0))
    lv_img_set_src(task_image, jpeg)
    visible(task_image, true)
    visible(title_label, false)
    visible(subtitle_label, false)
    visible(footer, false)
    lv_label_set_text(time_caption, "TIME LEFT")
    update_values()
    update_clock()
  end

  -- Animate reusable particles at 50 ms intervals and count real elapsed seconds.
  local function tick()
    local now = tmr.time()
    if scene.task and scene.paused then
      scene.last_second = now
      return
    end
    local elapsed = now - scene.last_second
    if elapsed > 0 then
      scene.last_second = now
      scene.counter = scene.counter + elapsed
      if scene.task and scene.looping then
        -- Keep overshoot when a callback crosses one or several round boundaries.
        scene.counter = scene.counter % scene.duration
        scene.reset_seconds = scene.duration - scene.counter
      else
        scene.reset_seconds = math.max(0, scene.reset_seconds - elapsed)
      end
      if scene.task then
        if scene.timed then
          scene.bp = math.floor(scene.initial_bp * (scene.reset_seconds / scene.duration) + 0.5)
          scene.remaining = scene.bp / 100
        end
        scene.counter = scene.duration - scene.reset_seconds
        update_values()
      end
      if scene.demo then
        local next_remaining = scene.remaining - 2 * elapsed
        if next_remaining < 0 then
          scene.remaining = 100
          scene.reset_seconds = scene.reset_initial
        else
          scene.remaining = next_remaining
        end
        update_values()
      end
      update_clock()
    end
    if scene.task and scene.reset_seconds == 0 then
      for _, grain in ipairs(grains) do visible(grain, false) end
      return
    end
    local span = math.max(2, scene.landing_y - 131)
    for index, grain in ipairs(grains) do
      local phase = ((scene.frames * 2 + index * 9) % 64) / 64
      local x = 79 + ((index + math.floor(scene.frames / 8)) % 3) - 1
      lv_obj_set_pos(grain, x, 128 + math.floor(phase * span))
    end
    scene.frames = scene.frames + 1
  end

  -- Settle elapsed time before pause/resume; reset starts the original task afresh.
  function scene.control(action)
    tick()
    if action == "reset" then
      scene.reset_seconds = scene.duration
      scene.bp = scene.initial_bp
      scene.remaining = scene.bp / 100
      scene.counter = 0
      scene.paused = false
    else
      scene.paused = action == "pause"
    end
    scene.last_second = tmr.time()
    update_values()
    update_clock()
  end

  -- Restore a validated checkpoint after an application update without shortening future loops.
  function scene.restore(counter, paused)
    scene.counter = counter
    scene.reset_seconds = scene.duration-counter
    scene.bp = scene.timed and math.floor(scene.initial_bp*(scene.reset_seconds/scene.duration)+0.5) or scene.initial_bp
    scene.remaining = scene.bp/100
    scene.paused = paused
    scene.last_second = tmr.time()
    update_values()
    update_clock()
  end

  -- Return lightweight verification data; counters keep advancing with USB closed.
  function scene.status()
    local result = {
      mode = scene.task and "task" or "quota", remaining = scene.remaining, reset_seconds = scene.reset_seconds,
      demo = scene.demo, frames = scene.frames, counter = scene.counter, updates = scene.updates,
    }
    if scene.task then
      result.mirror_protocol = 1
      result.bp = scene.bp
      result.initial_bp = scene.initial_bp
      result.paused = scene.paused
      result.duration = scene.duration
      result.timed = scene.timed
      result.looping = scene.looping
      result.accent = scene.accent
      result.labels_crc = scene.labels_crc
      result.labels_size = #scene.labels_jpeg
    end
    return result
  end

  scene.update(remaining, reset_seconds, demo)
  local timer = tmr.create()
  assert(timer:alarm(50, tmr.ALARM_AUTO, tick), "cannot start quota animation timer")

  -- Hide during playlist rotation without pausing or resetting the independent task clock.
  function scene.visible(enabled)
    visible(container, enabled)
  end

  -- Stop the owned timer before removing the scene's complete object subtree.
  function scene.stop()
    timer:unregister()
    lv_obj_del(container)
  end
  return scene
end

return create_quota_scene
