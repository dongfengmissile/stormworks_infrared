-- ir.flir must be called every tick
function onTick()
	on = input.getBool(1)
	ir.flir(on)
	ir.flir_config(1, 1, 1, false, 5, 80)
end

function onDraw()
	if on then screen.drawText(2, 2, "FLIR") end
end
