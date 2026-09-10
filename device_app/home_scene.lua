-- Device-local playlist clock and smooth fade-out / fade-in transitions.
local module = { transition_ms = 1200 }

-- Validate bounded display slots before creating any scene or timer.
function module.validate(entries)
  if type(entries) ~= "table" or #entries < 1 or #entries > 5 then return false end
  local seen = {}
  for _, entry in ipairs(entries) do
    if type(entry) ~= "table" or (entry.mode ~= "task" and entry.mode ~= "radar" and entry.mode ~= "text" and entry.mode ~= "image" and entry.mode ~= "calendar")
      or seen[entry.mode] or type(entry.seconds) ~= "number" or entry.seconds < 1 or entry.seconds > 3600
      or entry.seconds ~= math.floor(entry.seconds) then return false end
    seen[entry.mode] = true
  end
  return true
end

-- Project a bounded phase into current content and transition progress; hold time excludes transition.
function module.project(entries, elapsed_ms)
  local transition = #entries > 1 and module.transition_ms or 0
  local total = 0
  for _, entry in ipairs(entries) do total = total + entry.seconds*1000 + transition end
  local phase = elapsed_ms % total
  for index, entry in ipairs(entries) do
    local span = entry.seconds*1000 + transition
    if phase < span then
      local animation = phase < entry.seconds*1000 and -1 or (phase-entry.seconds*1000)/transition
      local selected = animation >= 0.5 and (index % #entries + 1) or index
      return entries[selected].mode, animation, total
    end
    phase = phase-span
  end
end

-- Smoothstep opacity reaches black at the switch midpoint, with zero slope at both ends.
function module.opacity(animation)
  local phase = animation <= 0.5 and animation*2 or (1-animation)*2
  phase = math.max(0, math.min(1, phase))
  return math.floor(phase*phase*(3-2*phase)*255+0.5)
end

-- Keep owned content alive; only show(mode) changes visibility and no task is restarted.
function module.create(parent, entries, session, show)
  assert(module.validate(entries), "invalid home playlist")
  for _,entry in ipairs(entries) do entry.seconds=math.floor(entry.seconds) end
  local scene = {elapsed=0, last=millis(), current=nil, entries=entries, session=session}
  local MAIN = LV_PART_MAIN
  local overlay = lv_obj_create(parent)
  lv_obj_set_pos(overlay,0,0); lv_obj_set_size(overlay,320,240)
  lv_obj_set_style_bg_color(overlay,0,MAIN)
  lv_obj_set_style_bg_opa(overlay,0,MAIN)
  lv_obj_set_style_border_width(overlay,0,MAIN); lv_obj_set_style_pad_all(overlay,0,MAIN)
  lv_obj_set_style_radius(overlay,0,MAIN); lv_obj_clear_flag(overlay,LV_OBJ_FLAG_SCROLLABLE)

  -- Advance with short monotonic deltas, safely crossing the firmware millisecond counter wrap.
  local function tick()
    local now = millis()
    local delta = (now-scene.last) & 0x7fffffff
    scene.last=now
    local _, _, total = module.project(entries,0)
    scene.elapsed=(scene.elapsed+delta)%total
    local mode, animation = module.project(entries,scene.elapsed)
    if mode ~= scene.current then scene.current=mode; show(mode) end
    -- Keep the overlay in the draw tree; changing HIDDEN at fade completion can
    -- invalidate the whole animated hourglass layer and cause a visible jump.
    local opacity=animation<0 and 0 or module.opacity(animation)
    if opacity~=scene.opacity then
      lv_obj_set_style_bg_opa(overlay,opacity,MAIN)
      scene.opacity=opacity
    end
  end

  -- Report only relative phase and the applied playlist, never the editor draft.
  function scene.status()
    return {mode="home",home_id=session,home_entries=entries,home_elapsed_ms=scene.elapsed,
      home_transition_ms=#entries>1 and module.transition_ms or 0,home_current=scene.current}
  end
  -- Stop rotation and remove the overlay while leaving the selected content intact.
  function scene.stop()
    scene.timer:unregister()
    lv_obj_del(overlay)
  end
  tick()
  scene.timer=tmr.create()
  assert(scene.timer:alarm(20,tmr.ALARM_AUTO,tick),"cannot start home timer")
  return scene
end
return module
