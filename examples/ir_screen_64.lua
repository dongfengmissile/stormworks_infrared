-- MC lua: draws the seeker's full-resolution IR image

AUTO_CONTRAST = true -- scale the brightest cell to white
GAIN = 1.0

cells = nil
frame_w = 0
frame_h = 0

function onTick()
	plugin_ok = (ir ~= nil and ir.frame ~= nil)
	if not plugin_ok then
		plugin_note = "no ir table here"
		cells = nil
		return
	end
	seeker_id = input.getNumber(3)
	if seeker_id == nil then
		seeker_id = 0
	end
	if seeker_id < 0 then
		plugin_ok = false
		plugin_note = "seeker reports no plugin"
		cells = nil
		return
	end
	cells, frame_w, frame_h = ir.frame(seeker_id)
end

function onDraw()
	screen.setColor(0, 0, 0)
	screen.drawClear()

	if not plugin_ok then
		screen.setColor(255, 0, 0)
		screen.drawTextBox(2, 2, screen.getWidth() - 4, screen.getHeight() - 4,
			"stormworks_infrared\nplugin not injected\n" .. (plugin_note or ""))
		return
	end

	if cells == nil or frame_w == nil or frame_h == nil then
		return
	end
	if frame_w < 1 or frame_h < 1 then
		return
	end

	scale = GAIN
	if AUTO_CONTRAST then
		peak = 0
		for i = 1, frame_w * frame_h do
			v = cells[i]
			if v ~= nil and v > peak then
				peak = v
			end
		end
		if peak > 0 then
			scale = 255.0 / peak
		end
	end

	sw = screen.getWidth()
	sh = screen.getHeight()
	cw = sw / frame_w
	ch = sh / frame_h

	for row = 0, frame_h - 1 do
		y = math.floor(row * ch)
		hh = math.max(1, math.floor((row + 1) * ch) - y)
		for col = 0, frame_w - 1 do
			v = cells[row * frame_w + col + 1]
			if v == nil then
				v = 0
			end
			g = math.floor(v * scale + 0.5)
			if g > 255 then
				g = 255
			end
			x = math.floor(col * cw)
			ww = math.max(1, math.floor((col + 1) * cw) - x)
			screen.setColor(g, g, g)
			screen.drawRectF(x, y, ww, hh)
		end
	end
end
