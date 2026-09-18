RES_X = 4
RES_Y = 4
PACKED = false

values = {}
detect = false
peak = 0

function onTick()
	-- decode resolution from the marker bits (LSB first)
	rx = 0
	ry = 0
	for b = 0, 2 do
		if input.getBool(2 + b) then
			rx = rx + 2 ^ b
		end
		if input.getBool(5 + b) then
			ry = ry + 2 ^ b
		end
	end
	if rx < 1 then
		rx = RES_X
	end
	if ry < 1 then
		ry = RES_Y
	end
	RES_X = rx
	RES_Y = ry
	CELLS = RES_X * RES_Y
	PACKED = input.getBool(8)

	nf = CELLS
	if PACKED then
		nf = (CELLS - CELLS % 2) / 2
		if CELLS % 2 == 1 then
			nf = nf + 1
		end
	end

	peak = 0
	for i = 1, nf do
		v = input.getNumber(i)
		if v == nil then
			v = 0
		end
		if PACKED then
			hi = math.floor(v / 4096)
			lo = v - hi * 4096
			values[i * 2 - 1] = lo / 4095
			if i * 2 <= CELLS then
				values[i * 2] = hi / 4095
			end
		else
			values[i] = v
		end
	end
	for i = 1, CELLS do
		if values[i] == nil then
			values[i] = 0
		end
		if values[i] < 0 then
			values[i] = 0
		end
		if values[i] > 1 then
			values[i] = 1
		end
		if values[i] > peak then
			peak = values[i]
		end
	end
end

function onDraw()
	sw = screen.getWidth()
	sh = screen.getHeight()

	screen.setColor(0, 0, 0)
	screen.drawClear()

	cw = sw / RES_X
	ch = sh / RES_Y
	for row = 0, RES_Y - 1 do
		for col = 0, RES_X - 1 do
			v = values[row * RES_X + col + 1]
			if v == nil then
				v = 0
			end
			g = math.floor(v * 255 + 0.5)
			screen.setColor(g, g, g)
			screen.drawRectF(math.floor(col * cw), math.floor(row * ch), math.ceil(cw), math.ceil(ch))
		end
	end
end
