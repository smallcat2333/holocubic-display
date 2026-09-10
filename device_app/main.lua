-- Receive JPEG frames or native quota-animation state over ESP32-S3 USB CDC.

local APP_KEY = "APP_USB_DISPLAY"
local previous = rawget(_G, APP_KEY)
if previous and previous.stop then
  pcall(function() previous.stop("reload") end)
end

local PREFIX = "@HCUSB/1 "
local APP_DIR = "/sd/apps/usb_display"
local MAX_IMAGE_BYTES = 512 * 1024
local EXPECTED_CHUNK_BYTES = 96
local root = lv_scr_act()
local create_quota_scene = dofile(APP_DIR .. "/quota_scene.lua")
local radar_module = dofile(APP_DIR .. "/radar_scene.lua")
local home_module = dofile(APP_DIR .. "/home_scene.lua")
local calendar_module = dofile(APP_DIR .. "/calendar_scene.lua")

local APP = {
  active_slot = "b",
  upload = nil,
  root = root,
  image = nil,
  status = nil,
  quota = nil,
}
_G[APP_KEY] = APP

-- Release the native animation before changing display modes or exiting the app.
local function stop_quota()
  if APP.calendar then APP.calendar.stop();APP.calendar=nil end
  APP.calendar_pending=nil
  APP.held_mode = nil
  if APP.home then APP.home.stop(); APP.home = nil end
  APP.home_images = nil
  if APP.radar then
    APP.radar.stop()
    APP.radar = nil
  end
  if APP.quota then
    APP.quota.stop()
    APP.quota = nil
  end
end

-- Encode a response with the firmware-provided JSON implementation.
local function json_encode(value)
  local codec = rawget(_G, "json") or rawget(_G, "sjson")
  return codec.encode(value)
end

-- Decode an untrusted host command and return nil on malformed JSON.
local function json_decode(value)
  local codec = rawget(_G, "json") or rawget(_G, "sjson")
  local ok, decoded = pcall(function() return codec.decode(value) end)
  if not ok or type(decoded) ~= "table" then return nil end
  return decoded
end

-- Write one protocol response without mixing it with ordinary debug logs.
local function send_response(id, status, values)
  local payload = values or {}
  payload.id = tostring(id or "")
  payload.status = status
  payload.app = "usb_display"
  local encoded = json_encode(payload)
  -- Firmware CDC writes can truncate a long argument. Keep each response bounded;
  -- the host pulls large composite states using the same verified short-line transport.
  if #encoded > 384 then
    APP.response_cache = {data=encoded,token=payload.id,created=tmr.time()}
    encoded = json_encode({id=payload.id,status="multipart",app="usb_display",size=#encoded,
      crc32=string.format("%08x",zlib.crc32(encoded,0))})
  end
  uart.write(0, PREFIX, encoded, "\n")
end

-- Change the centered waiting/error message and hide the image layer.
local function show_status(message, color)
  if not APP.preparing then stop_quota() end
  lv_obj_add_flag(APP.image, LV_OBJ_FLAG_HIDDEN)
  lv_obj_clear_flag(APP.status, LV_OBJ_FLAG_HIDDEN)
  lv_label_set_text(APP.status, tostring(message or ""))
  lv_obj_set_style_text_color(APP.status, color or 0xDDF8FF, LV_PART_MAIN)
end

-- Load the validated JPEG bytes so reused SD filenames cannot return cached pixels.
local function show_image(path)
  local jpeg = assert(file.getcontents(path), "cannot read completed JPEG")
  lv_img_set_src(APP.image, jpeg)
  lv_obj_set_pos(APP.image, 0, 0)
  lv_obj_add_flag(APP.status, LV_OBJ_FLAG_HIDDEN)
  lv_obj_clear_flag(APP.image, LV_OBJ_FLAG_HIDDEN)
  lv_obj_invalidate(APP.root)
end

-- Close and optionally remove an incomplete upload.
local function cancel_upload(remove_file)
  APP.radar_upload = nil
  local upload = APP.upload
  if not upload then return end
  if upload.file then
    pcall(function() upload.file:close() end)
  end
  if remove_file and upload.temp_path then
    pcall(function() file.remove(upload.temp_path) end)
  end
  APP.upload = nil
end

local BASE64_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
local BASE64_VALUES = {}
for index = 1, #BASE64_ALPHABET do
  BASE64_VALUES[BASE64_ALPHABET:sub(index, index)] = index - 1
end

-- Decode one small base64 chunk without retaining the full image in RAM.
local function base64_decode(value)
  if type(value) ~= "string" then return nil, "chunk data is not a string" end
  local output = {}
  local accumulator = 0
  local bit_count = 0
  for index = 1, #value do
    local char = value:sub(index, index)
    if char == "=" then break end
    local digit = BASE64_VALUES[char]
    if digit == nil then return nil, "invalid base64 data" end
    accumulator = accumulator * 64 + digit
    bit_count = bit_count + 6
    if bit_count >= 8 then
      bit_count = bit_count - 8
      local byte = math.floor(accumulator / (2 ^ bit_count)) % 256
      output[#output + 1] = string.char(byte)
      accumulator = accumulator % (2 ^ bit_count)
    end
  end
  return table.concat(output)
end

-- Encode a bounded readback chunk; only the active scene's label JPEG is exposed.
local function base64_encode(value)
  local output = {}
  for index = 1, #value, 3 do
    local a, b, c = value:byte(index, index + 2)
    local number = a * 65536 + (b or 0) * 256 + (c or 0)
    local d1 = math.floor(number / 262144) % 64 + 1
    local d2 = math.floor(number / 4096) % 64 + 1
    local d3 = math.floor(number / 64) % 64 + 1
    local d4 = number % 64 + 1
    output[#output + 1] = BASE64_ALPHABET:sub(d1, d1) .. BASE64_ALPHABET:sub(d2, d2)
      .. (b and BASE64_ALPHABET:sub(d3, d3) or "=") .. (c and BASE64_ALPHABET:sub(d4, d4) or "=")
  end
  return table.concat(output)
end

-- Start a bounded upload into the inactive frame slot.
local function begin_image(command)
  cancel_upload(true)
  local size = tonumber(command.size)
  local checksum = command.crc32
  if not size or size <= 0 or size > MAX_IMAGE_BYTES or size ~= math.floor(size) then
    return nil, "invalid image size"
  end
  if type(checksum) ~= "string" or #checksum ~= 8 or not checksum:match("^%x+$") then
    return nil, "crc32 must contain 8 hexadecimal digits"
  end

  local slot = APP.active_slot == "a" and "b" or "a"
  local temp_path = APP_DIR .. "/frame_" .. slot .. ".tmp"
  local final_path = APP_DIR .. "/frame_" .. slot .. ".jpg"
  local target = command.target or "frame"
  local calendar_bank,calendar_page=target:match("^calendar_([ab])_(%d%d)$")
  if calendar_bank then
    local pending=APP.calendar_pending
    calendar_page=tonumber(calendar_page)
    if not pending or pending.bank~=calendar_bank or calendar_page<1 or calendar_page>pending.count then
      return nil,"calendar_begin required or page out of range"
    end
    temp_path=APP_DIR.."/"..target..".tmp"
    final_path=APP_DIR.."/"..target..".jpg"
  elseif target == "task_labels" then
    if not APP.preparing then stop_quota() end
    temp_path = APP_DIR .. "/task_labels.tmp"
    final_path = APP_DIR .. "/task_labels.jpg"
  elseif target == "home_text" or target == "home_image" then
    if not APP.preparing then return nil, "home_begin required" end
    temp_path = APP_DIR .. "/" .. target .. ".tmp"
    final_path = APP_DIR .. "/" .. target .. ".jpg"
  elseif target ~= "frame" then
    return nil, "unsupported image target"
  end
  pcall(function() file.remove(temp_path) end)
  local descriptor = file.open(temp_path, "w+")
  if not descriptor then return nil, "cannot open temporary image" end

  APP.upload = {
    file = descriptor,
    temp_path = temp_path,
    final_path = final_path,
    slot = slot,
    target = target,
    calendar_page = calendar_page,
    expected_size = size,
    expected_crc = checksum:lower(),
    received = 0,
    crc = 0,
    next_sequence = 0,
  }
  if not calendar_bank then show_status("USB RECEIVING\n0%", 0x35E7FF) end
  return true
end

-- Append one ordered binary chunk and update its incremental CRC.
local function append_image_chunk(command)
  local upload = APP.upload
  if not upload then return nil, "image_begin required" end
  local sequence = tonumber(command.seq)
  if sequence == upload.next_sequence - 1 then return true end
  if sequence ~= upload.next_sequence then return nil, "chunk sequence mismatch" end

  local decoded, decode_error = base64_decode(command.data)
  if not decoded then return nil, decode_error end
  if #decoded == 0 or #decoded > EXPECTED_CHUNK_BYTES then return nil, "invalid chunk size" end
  if upload.received + #decoded > upload.expected_size then return nil, "image exceeds declared size" end

  local ok, write_result = pcall(function() return upload.file:write(decoded) end)
  if not ok or not write_result then return nil, "SD write failed" end
  upload.received = upload.received + #decoded
  -- Keep the native signed seed: this firmware uses 32-bit Lua integers.
  upload.crc = zlib.crc32(decoded, upload.crc)
  upload.next_sequence = upload.next_sequence + 1

  if upload.next_sequence % 20 == 0 then
    local percent = math.floor(upload.received * 100 / upload.expected_size)
    lv_label_set_text(APP.status, "USB RECEIVING\n" .. tostring(percent) .. "%")
  end
  return true
end

-- Validate, atomically publish, and display the completed JPEG.
local function finish_image()
  local upload = APP.upload
  if not upload then return nil, "image_begin required" end
  upload.file:flush()
  upload.file:close()
  upload.file = nil
  if upload.received ~= upload.expected_size then
    cancel_upload(true)
    return nil, "image size mismatch"
  end
  -- Hex strings preserve every bit even when lua_Number is only 32-bit float.
  local checksum = string.format("%08x", upload.crc)
  if checksum ~= upload.expected_crc then
    cancel_upload(true)
    return nil, "image crc32 mismatch"
  end

  pcall(function() file.remove(upload.final_path) end)
  local renamed = file.rename(upload.temp_path, upload.final_path)
  if not renamed then
    cancel_upload(true)
    return nil, "cannot publish image"
  end

  local result = {
    bytes = upload.received,
    crc32 = checksum,
    path = upload.final_path,
    target = upload.target,
  }
  if upload.target == "frame" then APP.active_slot = upload.slot end
  if upload.calendar_page then
    APP.calendar_pending.pages[upload.calendar_page]={path=upload.final_path,crc32=checksum,size=upload.received}
  end
  APP.upload = nil
  if result.target == "frame" then show_image(result.path) end
  return result
end

-- Include all retained scene states while the device, not the host, rotates their visibility.
local function current_state()
    local values = APP.home and APP.home.status() or (APP.calendar and APP.calendar.status() or (APP.radar and APP.radar.status() or (APP.quota and APP.quota.status() or { mode = "image" })))
    if APP.home then
      if APP.quota then values.task = APP.quota.status() end
      if APP.radar then values.radar = APP.radar.status() end
      if APP.calendar then values.calendar=APP.calendar.status() end
      if next(APP.home_images) then
        values.home_images = {}
        for mode, picture in pairs(APP.home_images) do
          values.home_images[mode] = {crc32=picture.crc32,size=#picture.data}
        end
      end
    end
    values.width = 320
    values.height = 240
    values.format = "jpeg"
    values.chunk_bytes = EXPECTED_CHUNK_BYTES
    values.receiving = APP.upload ~= nil
    values.task_protocol = 1
    values.mirror_protocol = 1
    values.loop_protocol = 1
    values.radar_protocol = 1
    values.radar_mirror_protocol = 1
    values.home_protocol = 1
    values.calendar_protocol = 1
    if APP.held_mode then values.held_mode=APP.held_mode end
    return values
end

-- Dispatch one decoded protocol command and acknowledge its result.
local function handle_command(command)
  local id = command.id or ""
  if command.cmd == "response_read" then
    local cached, offset = APP.response_cache, command.offset
    if not cached or command.token~=cached.token or tmr.time()-cached.created>20
      or type(offset)~="number" or offset~=math.floor(offset) or offset<0 or offset>=#cached.data then
      send_response(id,"error",{message="invalid or expired response chunk"}); return
    end
    send_response(id,"part",{offset=offset,data=base64_encode(cached.data:sub(offset+1,offset+96))})
  elseif command.cmd == "hello" or command.cmd == "status" then
    local values = current_state()
    send_response(id, "ready", values)
  elseif command.cmd == "calendar_begin" then
    if command.pages~=1
      or type(command.signature)~="string" or #command.signature~=16 or not command.signature:match("^%x+$")
      or type(command.date)~="string" or not command.date:match("^%d%d%d%d%-%d%d%-%d%d$")
      or type(command.total)~="number" or command.total~=math.floor(command.total) or command.total<0 or command.total>10000
      or type(command.done)~="number" or command.done~=math.floor(command.done) or command.done<0 or command.done>command.total then
      send_response(id,"error",{message="invalid calendar metadata"});return
    end
    cancel_upload(true)
    local bank=APP.calendar and APP.calendar.bank=="a" and "b" or "a"
    APP.calendar_pending={bank=bank,count=command.pages,pages={},signature=command.signature,date=command.date,total=command.total,done=command.done}
    send_response(id,"ok",{bank=bank})
  elseif command.cmd == "calendar_commit" then
    local pending=APP.calendar_pending
    if not pending or pending.signature~=command.signature then send_response(id,"error",{message="calendar_begin required"});return end
    local pages,total={},0
    for index=1,pending.count do
      local page=pending.pages[index]
      if not page then send_response(id,"error",{message="calendar page missing"});return end
      total=total+page.size
      if total>1048576 then send_response(id,"error",{message="calendar exceeds 1MB"});return end
      local data=file.getcontents(page.path)
      if not data or #data~=page.size or string.format("%08x",zlib.crc32(data,0))~=page.crc32 then
        send_response(id,"error",{message="calendar stored page mismatch"});return
      end
      pages[index]={data=data,crc32=page.crc32}
    end
    if APP.home and not APP.calendar and command.standalone~=true then
      send_response(id,"error",{message="calendar is not selected in home"});return
    end
    if command.standalone==true then APP.preparing=false;stop_quota() end
    if not APP.calendar then
      if not APP.preparing and not APP.home then stop_quota() end
      APP.calendar=calendar_module.create(APP.root)
    end
    pending.pages=pages
    APP.calendar.update(pending)
    APP.calendar_pending=nil
    if APP.home then APP.calendar.visible(APP.home.current=="calendar")
    else
      APP.calendar.visible(not APP.preparing)
      if not APP.preparing then lv_obj_add_flag(APP.image,LV_OBJ_FLAG_HIDDEN);lv_obj_add_flag(APP.status,LV_OBJ_FLAG_HIDDEN) end
    end
    send_response(id,"ok",APP.home and current_state() or APP.calendar.status())
  elseif command.cmd == "calendar_read" then
    local scene=APP.calendar
    local page=type(command.page)=="number" and command.page==math.floor(command.page) and scene and scene.pages[command.page]
    local offset=command.offset
    if not scene or command.signature~=scene.signature or not page or type(offset)~="number" or offset~=math.floor(offset) or offset<0 or offset>=#page.data then
      send_response(id,"error",{message="invalid calendar read"});return
    end
    send_response(id,"ok",{page=command.page,offset=offset,crc32=page.crc32,data=base64_encode(page.data:sub(offset+1,offset+192))})
  elseif command.cmd == "home_begin" then
    cancel_upload(true)
    stop_quota()
    APP.preparing = true
    show_status("HOME\nPreparing screens", 0x35E7FF)
    send_response(id, "ok")
  elseif command.cmd == "home_apply" then
    if not APP.preparing or not home_module.validate(command.entries)
      or type(command.session) ~= "string" or #command.session ~= 8 or not command.session:match("^%x+$") then
      send_response(id, "error", {message="invalid home preparation or playlist"}); return
    end
    local pictures = {}
    for _, entry in ipairs(command.entries) do
      if entry.mode == "task" and (not APP.quota or not APP.quota.task) then
        send_response(id, "error", {message="home task not prepared"}); return
      elseif entry.mode == "radar" and not APP.radar then
        send_response(id, "error", {message="home radar not prepared"}); return
      elseif entry.mode == "calendar" and not APP.calendar then
        send_response(id,"error",{message="home calendar not prepared"});return
      elseif entry.mode == "text" or entry.mode == "image" then
        local jpeg = file.getcontents(APP_DIR.."/home_"..entry.mode..".jpg")
        if not jpeg then send_response(id, "error", {message="home image not prepared"}); return end
        pictures[entry.mode] = {data=jpeg,crc32=string.format("%08x",zlib.crc32(jpeg,0))}
      end
    end
    APP.home_images = pictures
    APP.preparing = false
    lv_obj_add_flag(APP.status, LV_OBJ_FLAG_HIDDEN)
    APP.home = home_module.create(APP.root, command.entries, command.session, function(mode)
      if APP.quota then APP.quota.visible(mode=="task") end
      if APP.radar then APP.radar.visible(mode=="radar") end
      if APP.calendar then APP.calendar.visible(mode=="calendar") end
      if mode=="text" or mode=="image" then
        lv_img_set_src(APP.image, APP.home_images[mode].data)
        lv_obj_clear_flag(APP.image, LV_OBJ_FLAG_HIDDEN)
      else lv_obj_add_flag(APP.image, LV_OBJ_FLAG_HIDDEN) end
    end)
    send_response(id, "ok", current_state())
  elseif command.cmd == "home_stop" then
    if not APP.home then send_response(id, "error", {message="no active home playlist"}); return end
    local mode = APP.home.current
    APP.home.stop(); APP.home = nil
    APP.held_mode = mode
    if APP.quota and mode~="task" then APP.quota.stop(); APP.quota=nil end
    if APP.radar and mode~="radar" then APP.radar.stop(); APP.radar=nil end
    if APP.calendar and mode~="calendar" then APP.calendar.stop();APP.calendar=nil end
    send_response(id, "ok", current_state())
  elseif command.cmd == "home_asset_read" then
    local picture = APP.home and APP.home_images[command.kind]
    if not picture or command.crc32~=picture.crc32 then
      send_response(id, "error", {message="home picture changed; refresh status"}); return
    end
    local offset=command.offset
    if type(offset)~="number" or offset~=math.floor(offset) or offset<0 or offset>=#picture.data then
      send_response(id, "error", {message="invalid home picture offset"}); return
    end
    send_response(id, "ok", {crc32=picture.crc32,offset=offset,data=base64_encode(picture.data:sub(offset+1,offset+192))})
  elseif command.cmd == "radar_begin" then
    if type(command.size) ~= "number" or command.size < 1 or command.size > 8192 or command.size ~= math.floor(command.size)
      or type(command.crc32) ~= "string" or #command.crc32 ~= 8 or not command.crc32:match("^%x+$") then
      send_response(id, "error", {message="invalid radar size or checksum"}); return
    end
    cancel_upload(true)
    APP.radar_upload = {size=command.size, checksum=command.crc32:lower(), received=0, seq=0, chunks={}, crc=0, started=tmr.time()}
    send_response(id, "ok")
  elseif command.cmd == "radar_chunk" then
    local upload = APP.radar_upload
    if not upload or tmr.time()-upload.started > 20 then
      APP.radar_upload = nil
      send_response(id, "error", {message="radar_begin required or upload expired"}); return
    end
    local bytes = base64_decode(command.data)
    if command.seq ~= upload.seq or not bytes or #bytes < 1 or #bytes > 96 or upload.received+#bytes > upload.size then
      send_response(id, "error", {message="invalid radar chunk"}); return
    end
    upload.chunks[#upload.chunks+1] = bytes
    upload.received, upload.seq = upload.received+#bytes, upload.seq+1
    upload.crc = zlib.crc32(bytes, upload.crc)
    send_response(id, "ok", {next_seq=upload.seq})
  elseif command.cmd == "radar_end" then
    local upload = APP.radar_upload
    APP.radar_upload = nil
    if not upload or tmr.time()-upload.started > 20 or upload.received ~= upload.size
      or string.format("%08x", upload.crc) ~= upload.checksum then
      send_response(id, "error", {message="radar size, age or checksum mismatch"}); return
    end
    local payload = table.concat(upload.chunks)
    local data = json_decode(payload)
    if not radar_module.validate(data) then
      send_response(id, "error", {message="invalid radar snapshot"}); return
    end
    if command.standalone == true then APP.preparing=false; stop_quota() end
    if APP.home and not APP.radar then
      send_response(id, "error", {message="radar is not selected in home playlist"}); return
    end
    if not APP.radar then
      if not APP.preparing and not APP.home then stop_quota() end
      APP.radar = radar_module.create(APP.root)
    end
    APP.radar.update(data, upload.checksum, payload)
    if APP.home then APP.radar.visible(APP.home.current=="radar")
    else
      lv_obj_add_flag(APP.image, LV_OBJ_FLAG_HIDDEN)
      lv_obj_add_flag(APP.status, LV_OBJ_FLAG_HIDDEN)
      APP.radar.visible(not APP.preparing)
    end
    send_response(id, "ok", APP.home and current_state() or APP.radar.status())
  elseif command.cmd == "radar_read" then
    local scene = APP.radar
    if not scene or command.crc32 ~= scene.crc then
      send_response(id, "error", {message="active radar changed; refresh status"}); return
    end
    local offset = command.offset
    if type(offset) ~= "number" or offset ~= math.floor(offset) or offset < 0 or offset >= #scene.payload then
      send_response(id, "error", {message="invalid radar read offset"}); return
    end
    send_response(id, "ok", {crc32=scene.crc, offset=offset, data=base64_encode(scene.payload:sub(offset+1, offset+192))})
  elseif command.cmd == "task_labels_read" then
    local scene = APP.quota
    if not scene or not scene.task or command.crc32 ~= scene.labels_crc then
      send_response(id, "error", { message = "active task labels changed; refresh status" })
      return
    end
    local offset = command.offset
    if type(offset) ~= "number" or offset ~= math.floor(offset) or offset < 0 or offset >= #scene.labels_jpeg then
      send_response(id, "error", { message = "invalid label read offset" })
      return
    end
    local data = scene.labels_jpeg:sub(offset + 1, offset + 192)
    send_response(id, "ok", { crc32 = scene.labels_crc, offset = offset, data = base64_encode(data) })
  elseif command.cmd == "task" then
    if type(command.bp) ~= "number" or command.bp < 0 or command.bp > 10000 or command.bp ~= math.floor(command.bp) then
      send_response(id, "error", { message = "bp must be an integer from 0 to 10000" })
      return
    end
    if type(command.seconds) ~= "number" or command.seconds < 1 or command.seconds > 604800 or command.seconds ~= math.floor(command.seconds) then
      send_response(id, "error", { message = "seconds must be an integer from 1 to 604800" })
      return
    end
    if type(command.timed) ~= "boolean" or type(command.looping) ~= "boolean" or type(command.accent) ~= "number" or command.accent < 0 or command.accent > 16777215 or command.accent ~= math.floor(command.accent) then
      send_response(id, "error", { message = "invalid task style or timer mode" })
      return
    end
    if not file.exists(APP_DIR .. "/task_labels.jpg") then
      send_response(id, "error", { message = "upload task_labels first" })
      return
    end
    if command.restore ~= nil then
      local saved=command.restore
      if type(saved)~="table" or type(saved.counter)~="number" or saved.counter~=math.floor(saved.counter)
        or saved.counter<0 or saved.counter>command.seconds or type(saved.paused)~="boolean"
        or (command.looping and saved.counter==command.seconds) then
        send_response(id,"error",{message="invalid task checkpoint"}); return
      end
    end
    cancel_upload(true)
    if not APP.preparing then stop_quota() end
    APP.quota = create_quota_scene(APP.root, command.bp / 100, command.seconds, false)
    APP.quota.configure_task(command.seconds, command.bp, command.timed, command.accent, command.looping)
    if command.restore then APP.quota.restore(command.restore.counter,command.restore.paused) end
    APP.quota.visible(not APP.preparing)
    lv_obj_add_flag(APP.image, LV_OBJ_FLAG_HIDDEN)
    lv_obj_add_flag(APP.status, LV_OBJ_FLAG_HIDDEN)
    send_response(id, "ok", APP.quota.status())
  elseif command.cmd == "task_control" then
    if not APP.quota or not APP.quota.task then
      send_response(id, "error", { message = "no active task" })
      return
    end
    if command.action ~= "pause" and command.action ~= "resume" and command.action ~= "reset" then
      send_response(id, "error", { message = "unsupported task action" })
      return
    end
    APP.quota.control(command.action)
    send_response(id, "ok", APP.home and current_state() or APP.quota.status())
  elseif command.cmd == "quota" then
    local remaining, reset_seconds = command.remaining, command.reset_seconds
    if type(remaining) ~= "number" or remaining < 0 or remaining > 100 or remaining ~= math.floor(remaining) then
      send_response(id, "error", { message = "remaining must be an integer from 0 to 100" })
      return
    end
    if type(reset_seconds) ~= "number" or reset_seconds < 0 or reset_seconds > 604800 or reset_seconds ~= math.floor(reset_seconds) then
      send_response(id, "error", { message = "reset_seconds must be an integer from 0 to 604800" })
      return
    end
    if type(command.demo) ~= "boolean" then
      send_response(id, "error", { message = "demo must be boolean" })
      return
    end
    cancel_upload(true)
    if APP.home or APP.radar or (APP.quota and APP.quota.task) then stop_quota() end
    if APP.quota then
      APP.quota.update(remaining, reset_seconds, command.demo)
    else
      APP.quota = create_quota_scene(APP.root, remaining, reset_seconds, command.demo)
    end
    lv_obj_add_flag(APP.image, LV_OBJ_FLAG_HIDDEN)
    lv_obj_add_flag(APP.status, LV_OBJ_FLAG_HIDDEN)
    send_response(id, "ok", APP.quota.status())
  elseif command.cmd == "clear" then
    cancel_upload(true)
    APP.preparing = false
    show_status("USB DISPLAY\nWaiting for Python", 0xDDF8FF)
    send_response(id, "ok")
  elseif command.cmd == "image_begin" then
    local ok, err = begin_image(command)
    send_response(id, ok and "ok" or "error", ok and nil or { message = err })
  elseif command.cmd == "image_chunk" then
    local ok, err = append_image_chunk(command)
    send_response(id, ok and "ok" or "error", ok and { next_seq = APP.upload.next_sequence } or { message = err })
  elseif command.cmd == "image_end" then
    local result, err = finish_image()
    local status = result and (result.target == "frame" and "displayed" or "stored") or "error"
    send_response(id, status, result or { message = err })
  else
    send_response(id, "error", { message = "unknown command" })
  end
end

-- Filter debug traffic, parse one newline-delimited command, and dispatch it.
local function handle_uart_line(data)
  local line = tostring(data or ""):gsub("[\r\n]+$", "")
  if line:sub(1, #PREFIX) ~= PREFIX then return end
  local command = json_decode(line:sub(#PREFIX + 1))
  if not command then
    send_response("", "error", { message = "invalid json" })
    return
  end
  local ok, err = pcall(function() handle_command(command) end)
  if not ok then
    cancel_upload(true)
    APP.preparing = false
    show_status("USB ERROR\n" .. tostring(err), 0xFF6B7A)
    send_response(command.id or "", "error", { message = tostring(err) })
  end
end

-- Release UART, file, key, and screen resources when the app exits or reloads.
function APP.stop(reason)
  cancel_upload(true)
  stop_quota()
  pcall(function() uart.on("data") end)
  pcall(function() key.off() end)
  if APP.root then pcall(function() lv_obj_clean(APP.root) end) end
  if rawget(_G, APP_KEY) == APP then _G[APP_KEY] = nil end
end
APP.shutdown = APP.stop

lv_obj_clean(root)
lv_obj_set_style_bg_color(root, 0x02070B, LV_PART_MAIN)
lv_obj_set_style_bg_opa(root, 255, LV_PART_MAIN)

APP.image = lv_img_create(root)
lv_obj_set_pos(APP.image, 0, 0)
lv_obj_add_flag(APP.image, LV_OBJ_FLAG_HIDDEN)

APP.status = lv_label_create(root)
lv_obj_set_size(APP.status, 290, 80)
lv_obj_set_style_text_align(APP.status, LV_TEXT_ALIGN_CENTER, LV_PART_MAIN)
lv_obj_set_style_text_font(APP.status, LV_FONT_MONTSERRAT_20, LV_PART_MAIN)
lv_obj_align(APP.status, LV_ALIGN_CENTER, 0, 0)
show_status("USB DISPLAY\nWaiting for Python", 0xDDF8FF)

uart.setup(0, 115200, uart.DATABITS_8 or 8, uart.PARITY_NONE or 0, uart.STOPBITS_1 or 1, 0)
pcall(function() uart.on("data") end)
uart.on("data", "\n", handle_uart_line)

key.on(key.HOME, function(event_type)
  if event_type == key.SHORT then app.exit() end
end)

send_response("", "ready", { event = "started", width = 320, height = 240 })
