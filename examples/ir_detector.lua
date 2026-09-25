-- Component lua used on my IR seekers on the workshop
fov = 0.0125
range = 10000.0
az = 0.0
el = 0.0
gain = 0.01
dflux = 0.02
rx = 8
ry = 8
fin_gain = 1.0
occl = true
forward_axis = 2 -- 0=+X 1=-X 2=+Y 3=-Y 4=+Z 5=-Z

cells = nil
az_err = 0.0
el_err = 0.0
strength = 0.0
detect = false

function onAddToSimulation()
	plugin_error_reported = false
end
function onRemoveFromSimulation() end
function onParse() end
function onTick(tick_time)
	cfg = component.getInputLogicSlotComposite(0)
	if cfg ~= nil and cfg.float_values ~= nil then
		f = cfg.float_values
		if f[1] ~= nil and f[1] > 0 then fov = f[1] end
		if f[2] ~= nil and f[2] > 0 then range = f[2] end
		if f[3] ~= nil then az = f[3] end
		if f[4] ~= nil then el = f[4] end
		if f[5] ~= nil and f[5] > 0 then gain = f[5] end
		if f[6] ~= nil and f[6] > 0 then dflux = f[6] end
		if f[7] ~= nil and f[7] > 0 then rx = math.floor(f[7] + 0.5) end
		if f[8] ~= nil and f[8] > 0 then ry = math.floor(f[8] + 0.5) end
		if f[9] ~= nil and f[9] ~= 0 then fin_gain = f[9] end
	end
	if rx < 1 then rx = 1 end
	if rx > 64 then rx = 64 end
	if ry < 1 then ry = 1 end
	if ry > 64 then ry = 64 end

	if ir ~= nil and ir.scan ~= nil then
		cells, az_err, el_err, peak_abs, flux, ambient, n_sources, seeker_id =
			ir.scan(az, el, fov, range, rx, ry, gain, dflux, forward_axis, occl, false, false)
		if flux ~= nil then
			strength = flux / gain
			if strength > 1 then
				strength = 1
			end
			detect = flux >= dflux
		end
	else
		seeker_id = -1
		strength = -1
	end

	out = { float_values = {}, bool_values = {} }
	out.float_values[1] = az_err * fin_gain
	out.float_values[2] = el_err * fin_gain
	out.float_values[3] = seeker_id or 0
	out.float_values[4] = strength
	out.float_values[5] = flux or 0
	out.float_values[6] = ambient or 0
	out.float_values[7] = n_sources or 0
	out.bool_values[1] = detect

	component.setOutputLogicSlotComposite(0, out)
	component.setOutputLogicSlotFloat(0, flux)
	component.setOutputLogicSlotBool(0, detect)


	if seeker_id < 0 and not plugin_error_reported then
		plugin_error_reported = true
		if type(error) == "function" then
			error("stormworks_infrared plugin not injected")
		else
			stormworks_infrared_plugin_not_injected()
		end
	end
end

function onRender() end
