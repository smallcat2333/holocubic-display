-- Native 320x240 radar: bounded numeric snapshots, no JPEG streaming or SD writes.
local module = {}

-- Accept only finite, exactly representable protocol integers.
local function integer(value, low, high)
  return type(value) == "number" and value >= low and value <= high and value == math.floor(value)
end

-- Validate the entire snapshot before changing the live scene.
function module.validate(data)
  if type(data) ~= "table" or type(data.cards) ~= "table" or #data.cards > 10
    or type(data.series) ~= "table" or #data.series > 4
    or not integer(data.total, -1, 10000000) or not integer(data.reset, -1, 10000000)
    or not integer(data.age, 0, 30) or type(data.warn) ~= "boolean" or type(data.tasks) ~= "boolean" then
    return false
  end
  for _, card in ipairs(data.cards) do
    if type(card) ~= "table" or #card ~= 6 or type(card[1]) ~= "string" or #card[1] > 20
      or not integer(card[2], 0, 10) or not integer(card[3], 0, 16777215) then return false end
    for n = 4, 6 do if not integer(card[n], -1, 10000000) then return false end end
  end
  local total = 0
  for _, row in ipairs(data.series) do
    if type(row) ~= "table" or #row ~= 4 or type(row[1]) ~= "string" or #row[1] > 10
      or not integer(row[2], 0, 16777215) or not integer(row[3], 0, 10000000)
      or type(row[4]) ~= "table" or #row[4] ~= 24 then return false end
    local count = 0
    for _, value in ipairs(row[4]) do
      if not integer(value, 0, 10000000) then return false end
      count = count + value
    end
    if count ~= row[3] then return false end
    total = total + count
  end
  return (data.total == -1 and #data.series == 0) or data.total == total
end

-- Allocate stable LVGL objects once; updates only change labels, colors and points.
function module.create(parent)
  local MAIN, WHITE, MUTED = LV_PART_MAIN, 0xE1F8FF, 0x8296AA
  local scene = { updates = 0, frame = 8, second = -1, started = tmr.time() }
  local container = lv_obj_create(parent)
  lv_obj_set_pos(container, 0, 0)
  lv_obj_set_size(container, 320, 240)
  lv_obj_set_style_bg_color(container, 0, MAIN)
  lv_obj_set_style_bg_opa(container, 255, MAIN)
  lv_obj_set_style_border_width(container, 0, MAIN)
  lv_obj_set_style_pad_all(container, 0, MAIN)
  lv_obj_set_style_radius(container, 0, MAIN)
  lv_obj_clear_flag(container, LV_OBJ_FLAG_SCROLLABLE)

  -- Fixed ASCII labels use fonts already verified on this firmware.
  local function label(text, x, y, width, height, font, color)
    local obj = lv_label_create(container)
    lv_obj_set_pos(obj, x, y)
    lv_obj_set_size(obj, width, height)
    lv_obj_set_style_text_font(obj, font, MAIN)
    lv_obj_set_style_text_color(obj, color, MAIN)
    lv_label_set_text(obj, text)
    return obj
  end

  -- Native polylines support both trend strokes and the segmented donut ring.
  local function line(points, width, color)
    local obj = lv_line_create(container)
    lv_obj_set_pos(obj, 0, 0)
    lv_line_set_points(obj, points)
    lv_obj_set_style_line_width(obj, width, MAIN)
    lv_obj_set_style_line_color(obj, color, MAIN)
    return obj
  end

  -- Hide unused widgets without deleting the objects during a refresh.
  local function visible(obj, yes)
    if yes then lv_obj_clear_flag(obj, LV_OBJ_FLAG_HIDDEN)
    else lv_obj_add_flag(obj, LV_OBJ_FLAG_HIDDEN) end
  end

  label("AI RADAR", 8, 5, 160, 24, LV_FONT_MONTSERRAT_20, WHITE)
  local badge = label("USB", 220, 10, 96, 14, LV_FONT_MONTSERRAT_10, MUTED)
  line({{x=8,y=30},{x=312,y=30}}, 1, 0x253547)
  line({{x=153,y=37},{x=153,y=194}}, 1, 0x253547)
  label("CODE / LAST 10 TASKS", 8, 34, 143, 15, LV_FONT_MONTSERRAT_10, MUTED)
  label("REQUESTS / 24H", 163, 34, 151, 15, LV_FONT_MONTSERRAT_10, MUTED)
  local cards = {}
  for index = 1, 2 do
    local y = 52 + (index - 1) * 70
    cards[index] = {
      label("--", 8, y, 141, 14, LV_FONT_MONTSERRAT_10, WHITE),
      label("--", 9, y + 14, 91, 35, LV_FONT_MONTSERRAT_28, WHITE),
      label("", 108, y + 21, 39, 16, LV_FONT_MONTSERRAT_12, MUTED),
      label("", 9, y + 50, 140, 15, LV_FONT_MONTSERRAT_10, MUTED)
    }
  end
  local page_label = label("", 8, 192, 143, 13, LV_FONT_MONTSERRAT_10, MUTED)
  local trend = {}
  for index = 1, 4 do
    trend[index] = line({{x=164,y=96},{x=311,y=96}}, 2, MUTED)
    visible(trend[index], false)
  end
  local total_label = label("-- requests", 163, 102, 151, 18, LV_FONT_MONTSERRAT_12, WHITE)
  local ring = {}
  for index = 1, 64 do
    local angle = (index - 0.5) * math.pi * 2 / 64 - math.pi / 2
    ring[index] = line({
      {x=math.floor(192 + math.cos(angle)*17),y=math.floor(152 + math.sin(angle)*17)},
      {x=math.floor(192 + math.cos(angle)*27),y=math.floor(152 + math.sin(angle)*27)}
    }, 3, 0x253547)
  end
  local legends = {}
  for index = 1, 4 do
    legends[index] = label("", 224, 123 + (index-1)*15, 92, 14, LV_FONT_MONTSERRAT_10, MUTED)
  end
  line({{x=8,y=208},{x=312,y=208}}, 1, 0x253547)
  label("RESET", 8, 217, 51, 17, LV_FONT_MONTSERRAT_12, 0x24D7EA)
  local reset_label = label("NO DATA", 65, 217, 249, 17, LV_FONT_MONTSERRAT_12, WHITE)

  -- Locate the old series by identity, not rank, when models reorder.
  local function old_row(rows, name)
    for _, row in ipairs(rows) do if row[1] == name then return row end end
    return nil
  end

  -- Interpolate known values only; an unknown score remains a dash.
  local function blend(a, b, progress)
    if not a or a < 0 then return b end
    return a + (b - a) * progress
  end

  -- Paint at 20fps for 400ms, then only once a second for age and countdown.
  local function paint()
    if not scene.data then return end
    local now = tmr.time()
    if scene.frame >= 8 and scene.second == now then return end
    scene.second = now
    scene.frame = math.min(8, scene.frame + 1)
    local t = scene.frame / 8
    t = t*t*(3-2*t)
    local data = scene.data
    local previous = scene.previous or data
    local age = now - scene.received + data.age
    lv_label_set_text(badge, age > 15 and ("STALE "..age.."s") or (data.warn and "SOURCE WARN" or "USB LIVE"))
    local pages = math.max(1, math.ceil(#data.cards/2))
    local page = math.floor((now-scene.started)/6) % pages
    for index, widgets in ipairs(cards) do
      local card = data.cards[page*2 + index]
      for _, obj in ipairs(widgets) do visible(obj, card ~= nil) end
      if card then
        local old = old_row(previous.cards, card[1])
        lv_label_set_text(widgets[1], card[1])
        lv_label_set_text(widgets[2], card[4] < 0 and "--" or string.format("%.0f", blend(old and old[4], card[4], t)/100))
        lv_label_set_text(widgets[3], "x"..card[2])
        lv_obj_set_style_text_color(widgets[2], card[3], MAIN)
        lv_obj_set_style_text_color(widgets[1], card[3], MAIN)
        local price = card[5] < 0 and "$--" or string.format("$%.2f", card[5]/100)
        local minutes = card[6] < 0 and "--m" or string.format("%.0fm", card[6]/10)
        lv_label_set_text(widgets[4], price.."   "..minutes)
      end
    end
    lv_label_set_text(page_label, not data.tasks and "TASK DATA UNAVAILABLE" or (#data.cards == 0 and "NO TASKS" or ("CODE  "..(page+1).."/"..pages)))
    local peak, counts, sum = 1, {}, 0
    for index, row in ipairs(data.series) do
      local old = old_row(previous.series, row[1])
      counts[index] = blend(old and old[3], row[3], t)
      sum = sum + counts[index]
      for n, value in ipairs(row[4]) do peak = math.max(peak, blend(old and old[4][n], value, t)) end
    end
    for index = 1, 4 do
      local row = data.series[index]
      visible(trend[index], row ~= nil)
      visible(legends[index], row ~= nil)
      if row then
        local old = old_row(previous.series, row[1])
        local points = {}
        for n, value in ipairs(row[4]) do
          points[n] = {x=164+math.floor((n-1)*147/23),y=96-math.floor(43*blend(old and old[4][n], value, t)/peak)}
        end
        lv_line_set_points(trend[index], points)
        lv_obj_set_style_line_color(trend[index], row[2], MAIN)
        lv_obj_set_style_text_color(legends[index], row[2], MAIN)
        lv_label_set_text(legends[index], row[1].." "..string.format("%.0f%%", sum > 0 and counts[index]*100/sum or 0))
      end
    end
    for index, obj in ipairs(ring) do
      local position, cumulative, color = (index-0.5)/64*sum, 0, 0x253547
      for n, row in ipairs(data.series) do
        cumulative = cumulative + counts[n]
        if sum > 0 and position < cumulative then color = row[2]; break end
      end
      lv_obj_set_style_line_color(obj, color, MAIN)
    end
    lv_label_set_text(total_label, data.total < 0 and "USAGE UNAVAILABLE" or string.format("%.0f requests", sum))
    local seconds = data.reset - (now-scene.received)
    local reset_text = "NO DEADLINE / SEE PC"
    if data.reset >= 0 then
      reset_text = seconds <= 0 and "WINDOW ENDED / CHECK PC" or string.format("EST. %02d:%02d:%02d", math.floor(seconds/3600), math.floor(seconds/60)%60, seconds%60)
    end
    lv_label_set_text(reset_label, reset_text)
  end

  -- Atomically install a validated snapshot and restart its short transition.
  function scene.update(data, crc, payload)
    scene.previous, scene.data = scene.data, data
    scene.payload = payload
    scene.received, scene.crc = tmr.time(), crc
    scene.updates, scene.frame = scene.updates + 1, 0
    paint()
  end

  -- Small ACK exposes identity and freshness, never returns private task content.
  function scene.status()
    return {mode="radar", radar_protocol=1, radar_mirror_protocol=1, crc32=scene.crc, updates=scene.updates,
      radar_size=#scene.payload, scene_seconds=tmr.time()-scene.started,
      age=scene.data and (tmr.time()-scene.received+scene.data.age) or 0,
      total=scene.data and scene.data.total or -1}
  end

  local timer = tmr.create()
  assert(timer:alarm(50, tmr.ALARM_AUTO, paint), "cannot start radar timer")
  -- Hide only the visual container; data freshness and native clock continue in the background.
  function scene.visible(enabled)
    visible(container, enabled)
  end

  -- Release the timer before deleting objects when switching back to other modes.
  function scene.stop()
    timer:unregister()
    lv_obj_del(container)
  end
  return scene
end

return module
