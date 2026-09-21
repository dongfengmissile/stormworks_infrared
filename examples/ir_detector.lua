-- Component lua, used on my published IR seekers
NODE_CONFIG   = 0
NODE_DATA     = 0
NODE_STRENGTH = 0
NODE_DETECT   = 0
RES_X = 16
RES_Y = 16
FOV_TURNS = 0.0125
RANGE_M = 5000.0
IR_GAIN = 0.005
DETECT_FLUX = 0.001
OCCLUSION = true
FORWARD_AXIS = 2 -- 0=+X 1=-X 2=+Y 3=-Y 4=+Z 5=-Z

function onAddToSimulation() end
function onRemoveFromSimulation() end
function onParse() end

function onTick(tick_time)
	fov   = FOV_TURNS
	range = RANGE_M
	az    = 0.0
	el    = 0.0
	gain  = IR_GAIN
	dflux = DETECT_FLUX
	rx    = RES_X
	ry    = RES_Y
	seeker_id = 0
	occl  = 1.0
	if not OCCLUSION then
		occl = 0.0
	end

	cfg = component.getInputLogicSlotComposite(NODE_CONFIG)
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
	end
	if rx < 1 then rx = 1 end
	if rx > 64 then rx = 64 end
	if ry < 1 then ry = 1 end
	if ry > 64 then ry = 64 end

	cells    = nil
	az_err   = 0.0
	el_err   = 0.0
	strength = 0.0
	detect   = false

	if ir ~= nil and ir.scan ~= nil then
		cells, az_err, el_err, peak_abs, flux, ambient, n_sources, seeker_id =
			ir.scan(az, el, fov, range, rx, ry, gain, dflux, FORWARD_AXIS, occl, false, false)
		if flux ~= nil then
			strength = flux / gain
			if strength > 1 then
				strength = 1
			end
			detect = flux >= dflux
		end
	end

	out = { float_values = {}, bool_values = {} }
	out.float_values[1] = seeker_id or 0
	out.float_values[2] = az_err
	out.float_values[3] = el_err
	out.float_values[4] = strength
	out.float_values[5] = flux or 0
	out.float_values[6] = ambient or 0
	out.float_values[7] = n_sources or 0
	out.bool_values[1] = detect

	component.setOutputLogicSlotComposite(NODE_DATA, out)
	component.setOutputLogicSlotFloat(NODE_STRENGTH, strength)
	component.setOutputLogicSlotBool(NODE_DETECT, detect)
end

function onRender() end
