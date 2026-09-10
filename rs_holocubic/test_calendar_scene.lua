-- Reuse the verified LVGL object mocks and existing scene lifecycle checks.
dofile("test_home_scene.lua")
local now,timer=0,nil
function millis() return now end
tmr={ALARM_AUTO=1,create=function()
  timer={alarm=function(self,ms,mode,callback) self.callback=callback;return true end,
    unregister=function(self) self.stopped=true end}
  return timer
end}
local calendar=dofile("../device_app/calendar_scene.lua").create({})
calendar.update({pages={{data="one",crc32="11111111"}},
  bank="a",signature="1234567890abcdef",date="2026-09-08",total=4,done=1})
assert(calendar.page==1 and calendar.status().calendar_done==1)
calendar.visible(false)
now=8100
assert(timer==nil and calendar.page==1 and calendar.status().calendar_elapsed_ms==0)
calendar.visible(true)
assert(calendar.page==1,"calendar must stay on the first page")
calendar.update({pages={{data="new",crc32="33333333"}},bank="b",signature="abcdef1234567890",date="2026-09-09",total=0,done=0})
assert(calendar.page==1 and calendar.status().calendar_date=="2026-09-09")
calendar.stop()
print("Lua calendar fixed first page, no timer, replacement and lifecycle: PASS")
