delay = 8 -- ticks of delay between seeker and warhead
min_flux = 1
prev_flux = 0
detonate = false

function onTick()
    flux = input.getNumber(5)
    armed = input.getBool(2)
    if armed and flux > min_flux and prev_flux > min_flux and flux > prev_flux then
        local r = math.sqrt(prev_flux / flux)
        if r / (1 - r) <= delay then detonate = true end
    end
    prev_flux = flux
    output.setBool(1, detonate)
end
