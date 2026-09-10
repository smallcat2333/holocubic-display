-- Reuse the checked LVGL mocks; this also runs the existing radar lifecycle assertions.
dofile("test_radar_scene.lua")
local home = dofile("../device_app/home_scene.lua")
local entries={{mode="task",seconds=2},{mode="radar",seconds=2}}
assert(home.validate(entries))
assert(not home.validate({}))
assert(not home.validate({{mode="task",seconds=0}}))
assert(not home.validate({entries[1],entries[1]}))
local mode,animation=home.project(entries,1999)
assert(mode=="task" and animation==-1)
mode,animation=home.project(entries,2600)
assert(mode=="radar" and animation==0.5)
mode,animation=home.project(entries,6400)
assert(mode=="task" and animation==-1)
assert(home.opacity(0)==0 and home.opacity(0.5)==255 and home.opacity(1)==0)
assert(home.opacity(0.25)==128 and home.opacity(0.75)==128)

local now,timers=0,{}
-- Simulated clocks dispatch both retained scenes while the host has no connection.
function millis() return now end
tmr={ALARM_AUTO=1,time=function() return math.floor(now/1000) end,create=function()
  local timer={alarm=function(self,ms,mode,callback) self.callback=callback;return true end,
    unregister=function(self) self.stopped=true end}
  timers[#timers+1]=timer
  return timer
end}
lv_img_create=lv_obj_create
-- Stub binary resources; pixel decoding is verified separately against real hardware.
function lv_img_set_src(obj,data) obj.source=data end
file={getcontents=function() return "labels" end}
zlib={crc32=function() return 1234 end}
local quota=dofile("../device_app/quota_scene.lua")({},100,20,false)
quota.configure_task(20,10000,true,0x35e7ff,false)
local playlist=home.create({},entries,"12345678",function(mode) quota.visible(mode=="task") end)
-- Advance all timers without issuing any USB command or recreating the task.
local function advance(ms)
  now=ms
  for _,timer in ipairs(timers) do if not timer.stopped then timer.callback() end end
end
advance(2600);assert(playlist.status().home_current=="radar")
assert(playlist.opacity==255)
assert(quota.status().counter==2)
advance(6400);assert(playlist.status().home_current=="task")
assert(playlist.opacity==0,"idle transition overlay must stay transparent")
assert(quota.status().counter==6 and quota.status().reset_seconds==14)
playlist.stop();advance(8000)
assert(quota.status().counter==8,"stopping rotation must not reset the selected task")
quota.restore(5,true)
advance(9000)
assert(quota.status().counter==5 and quota.status().duration==20 and quota.status().paused)
quota.stop()
print("Lua home phase, smooth fade, background task and stop: PASS")
