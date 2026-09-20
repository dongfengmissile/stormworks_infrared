-- Component Lua, not for use in stormworks MCs
NODE_CONFIG   = 0
NODE_ARRAY    = 0
NODE_STRENGTH = 0
NODE_DETECT   = 0

RES_X = 4
RES_Y = 4 -- cells = RES_X*RES_Y; 33->64 cells auto-encode as 2 cells/float (12-bit each) so one composite still carries them (see ir_array_to_screen for decoding)
FOV_TURNS = 0.0125
RANGE_M = 1000.0 -- max detection distance (m)
IR_GAIN = 0.005 -- total flux that maps to full-scale white
DETECT_FLUX = 0.001 -- total flux needed for Target Detect
OCCLUSION = true -- line-of-sight check through the physics world
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
	end

	cells    = nil
	az_err   = 0.0
	el_err   = 0.0
	strength = 0.0
	detect   = false

	if ir ~= nil and ir.scan ~= nil then
		pack = RES_X * RES_Y > 32
		cells, az_err, el_err, peak_abs, flux, ambient =
			ir.scan(az, el, fov, range, RES_X, RES_Y, gain, dflux, FORWARD_AXIS, occl, pack)
		-- Strength is the received IR flux over the gain (0->1); Detect is an absolute threshold on the received IR flux.
		if flux ~= nil then
			strength = flux / gain
			if strength > 1 then
				strength = 1
			end
			detect = flux >= dflux
		end
	end

	n = RES_X * RES_Y
	out = { float_values = {}, bool_values = {} }
	if cells ~= nil then
		-- raw mode: floats 1->n are the cell intensities; packed mode: the plugin returns ceil(n/2) floats, each = high*4096 + low (two 12-bit cells).
		for i = 1, 32 do
			if cells[i] ~= nil then
				out.float_values[i] = cells[i]
			end
		end
	end

	-- Composite bool channels let a monitor/micro decode the format:
	--   bool 1        target detect
	--   bool 2->4     RES_X bits (LSB first)
	--   bool 5->7     RES_Y bits (LSB first)
	--   bool 8        cells are packed 2-per-float (12-bit each)
	out.bool_values[1] = detect
	out.bool_values[2] = RES_X % 2 >= 1
	out.bool_values[3] = RES_X % 4 >= 2
	out.bool_values[4] = RES_X % 8 >= 4
	out.bool_values[5] = RES_Y % 2 >= 1
	out.bool_values[6] = RES_Y % 4 >= 2
	out.bool_values[7] = RES_Y % 8 >= 4
	out.bool_values[8] = n > 32

	component.setOutputLogicSlotComposite(NODE_ARRAY, out)
	component.setOutputLogicSlotFloat(NODE_STRENGTH, strength)
	component.setOutputLogicSlotBool(NODE_DETECT, detect)
end

function onRender() end
