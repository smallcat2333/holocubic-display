-- Run with Lua 5.3+ from rs_holocubic; mocks validate state/layout, not physical pixels.
local now, objects, redraws = 100, {}, 0
LV_PART_MAIN, LV_OBJ_FLAG_HIDDEN, LV_OBJ_FLAG_SCROLLABLE = 0, 1, 2
LV_FONT_MONTSERRAT_10, LV_FONT_MONTSERRAT_12, LV_FONT_MONTSERRAT_20, LV_FONT_MONTSERRAT_28 = 10, 12, 20, 28

-- Every native object is retained for assertions on size, text and lifecycle.
local function object(parent)
  local obj = {parent=parent}
  objects[#objects+1] = obj
  return obj
end
lv_obj_create, lv_label_create, lv_line_create = object, object, object
-- The simulator records geometry without claiming to reproduce LVGL font rasterization.
function lv_obj_set_pos(obj,x,y) obj.x,obj.y=x,y end
function lv_obj_set_size(obj,w,h) obj.w,obj.h=w,h end
function lv_obj_set_style_text_font(obj,font) obj.font=font end
function lv_label_set_text(obj,text) obj.text=text; redraws=redraws+1 end
function lv_line_set_points(obj,points)
  for _,point in ipairs(points) do assert(point.x>=0 and point.x<320 and point.y>=0 and point.y<240) end
  obj.points=points
end
function lv_obj_add_flag(obj,flag) if flag==LV_OBJ_FLAG_HIDDEN then obj.hidden=true end end
function lv_obj_clear_flag(obj,flag) if flag==LV_OBJ_FLAG_HIDDEN then obj.hidden=false end end
function lv_obj_del(obj) obj.deleted=true end
-- Style setters are no-ops; the real firmware remains the authority for rendering.
local function style() end
for _,name in ipairs({"bg_color","bg_opa","border_width","pad_all","radius","text_color","line_width","line_color"}) do
  _G["lv_obj_set_style_"..name] = style
end
local timer
tmr = {ALARM_AUTO=1, time=function() return now end,
  create=function()
    timer={alarm=function(self,ms,mode,callback) self.callback=callback; return true end,
      unregister=function(self) self.stopped=true end}
    return timer
  end}

local radar = dofile("../device_app/radar_scene.lua")
assert(loadfile("../device_app/main.lua"))
local bins = {}
for n=1,24 do bins[n]=n end
local data = {cards={{"Astra max",10,0xFF7E1D,10650,-1,250}},
  series={{"Astra",0xFF7E1D,300,bins}}, total=300, reset=5, age=0,warn=false,tasks=true}
assert(radar.validate(data))
data.total=301; assert(not radar.validate(data)); data.total=300
assert(not radar.validate({}))
local scene=radar.create({})
scene.update(data,"12345678","test")
assert(scene.status().mode=="radar" and scene.status().total==300)
assert(scene.status().crc32=="12345678")
assert(scene.status().radar_size==4 and scene.status().scene_seconds==0)
for _,obj in ipairs(objects) do assert(obj.text ~= "EFFORT: NOT LOGGED") end
for n=1,10 do timer.callback() end
local previous=redraws
timer.callback(); assert(redraws==previous,"settled frame should not redraw at 20fps")
now=117; timer.callback()
local stale, overdue=false,false
for _,obj in ipairs(objects) do
  if obj.text=="STALE 17s" then stale=true end
  if obj.text=="WINDOW ENDED / CHECK PC" then overdue=true end
end
assert(stale and overdue,"host loss and deadline expiry must be explicit")
scene.update({cards={},series={},total=-1,reset=-1,age=0,warn=true,tasks=false},"abcdef12","test2")
assert(scene.status().updates==2 and scene.status().total==-1)
scene.stop()
assert(timer.stopped and objects[1].deleted,"switching modes must release timer and container")
print("Lua radar validation, animation, stale data, expiry and lifecycle: PASS")
