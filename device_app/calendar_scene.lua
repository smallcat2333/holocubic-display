-- CalendarTask keeps the first page fixed; only changed source content replaces it.
local module = {page_ms=0}

-- Own one image container and a bounded set of verified JPEG pages.
function module.create(parent)
  local MAIN=LV_PART_MAIN
  local container=lv_obj_create(parent)
  lv_obj_set_pos(container,0,0);lv_obj_set_size(container,320,240)
  lv_obj_set_style_bg_color(container,0,MAIN);lv_obj_set_style_bg_opa(container,255,MAIN)
  lv_obj_set_style_border_width(container,0,MAIN);lv_obj_set_style_pad_all(container,0,MAIN)
  lv_obj_set_style_radius(container,0,MAIN);lv_obj_clear_flag(container,LV_OBJ_FLAG_SCROLLABLE)
  local picture=lv_img_create(container)
  lv_obj_set_pos(picture,0,0)
  local scene={elapsed=0,page=1}

  -- Publish a complete bank atomically; no partly received page becomes visible.
  function scene.update(bundle)
    assert(#bundle.pages==1,"calendar requires exactly the first page")
    scene.pages=bundle.pages;scene.bank=bundle.bank;scene.signature=bundle.signature
    scene.date=bundle.date;scene.total=bundle.total;scene.done=bundle.done
    scene.elapsed=0;scene.page=1
    lv_img_set_src(picture,scene.pages[1].data)
  end

  -- Expose immutable page identities and the local paging phase for actual desktop previews.
  function scene.status()
    local pages={}
    for index,page in ipairs(scene.pages) do pages[index]={crc32=page.crc32,size=#page.data} end
    return {mode="calendar",calendar_protocol=1,calendar_signature=scene.signature,
      calendar_date=scene.date,calendar_total=scene.total,calendar_done=scene.done,
      calendar_pages=pages,calendar_elapsed_ms=scene.elapsed,calendar_page_ms=module.page_ms}
  end

  -- Hiding the calendar does not change its first page.
  function scene.visible(enabled)
    if enabled then lv_obj_clear_flag(container,LV_OBJ_FLAG_HIDDEN)
    else lv_obj_add_flag(container,LV_OBJ_FLAG_HIDDEN) end
  end
  -- No paging timer is allocated; release the complete container on exit.
  function scene.stop()
    lv_obj_del(container)
  end
  return scene
end
return module
