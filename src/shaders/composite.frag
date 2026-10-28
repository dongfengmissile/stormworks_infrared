#version 330 core
in vec2 uv;
out vec4 color_out;

uniform sampler2D tex_normal;
uniform sampler2D tex_color;
uniform sampler2D tex_depth;
uniform int has_depth;
uniform int normal_encoded;

uniform vec3 sun_dir;
uniform float sun_valid;
uniform float day_pct;
uniform float clear_sky;
// Deferred light pass state of this view: inverse view-projection (render
// space) and the sea plane's render-space y (world y 0 - graphics_offset.y).
uniform mat4 mat_inv;
uniform int has_inv;
uniform float water_y;
uniform float ambient_c;      // sea-level air: terrain, water, people
uniform float vehicle_air_c;  // air at the vehicles/camera
uniform sampler2D tex_water;  // game's ocean-surface log depth (depth_ocean)
uniform int has_water;
uniform vec4 plume_a[8];      // jet plume start (render space), excess C
uniform vec4 plume_b[8];      // unit flow direction, nozzle radius
uniform int plume_n;
const float DETAIL = 0.12;   // detail overlay strength in display units
uniform float sun_soak_c;
uniform vec3 tint;
uniform int black_hot;
uniform float tmin;
uniform float tmax;
uniform float enc_min;
uniform float enc_max;
uniform float sky_c;
uniform float sky_depth_m;

// depth_utils.glslh: log depth, near 0.025, far 20100, C 0.01
float decode_linear_depth(float d)
{
    const float FC = 7.658227526203386; // log2(20100.0 * 0.01 + 1.0)
    return (pow(2.0, d * FC) - 1.0) * 100.0;
}

// Attachment 0 is gnormal_light_factor: a full WORLD-space normal in xyz and the
// light factor in .w (`vec4(normalize(vertex_normal_out), light_factor)` in
// opaque.glslf; the game's light pass reads normalize(gnormal.xyz) and gnormal.a).
// z is a real normal component - reconstructing it from xy would lose the sign
// and give every downward-facing surface the wrong sun soak.
vec3 gbuffer_normal(vec4 raw)
{
    if (normal_encoded == 0)
    {
        float len2 = dot(raw.xyz, raw.xyz);
        // NaN-safe: an unwritten normal falls back to up instead of garbage.
        return len2 > 1e-8 ? raw.xyz * inversesqrt(len2) : vec3(0.0, 1.0, 0.0);
    }
    // Normalized-attachment fallback: packed xy, z reconstructed (positive).
    vec2 nxy = raw.xy * 2.0 - 1.0;
    float len2 = dot(nxy, nxy);
    if (!(len2 <= 1.0))
    {
        return vec3(0.0, 1.0, 0.0);
    }
    return vec3(nxy, sqrt(1.0 - len2));
}

const float TWO_PI = 6.283185307;

// FlightGear Advanced Weather diurnal surface cycle (Nasal/local_weather/
// local_weather.nas): f = 0.5 * (1 - cos(day * 2pi - lag)), lag in radians =
// thermal inertia (soil 0.4, water 0.9, vegetation/rock 0).
float diurnal(float lag)
{
    return 0.5 * (1.0 - cos(day_pct * TWO_PI - lag));
}

// Terrain/props surface temperature from the G-buffer albedo (linear gcolor):
// - material class from colour (green = vegetation, otherwise soil/rock),
//   with FlightGear's delta-T cycles (veg -5..+5, soil -5..+5 lagged,
//   rock -7..+7) scaled by the clear-sky factor;
// - absorbed sun = (1 - albedo) * incidence: FlightGear's IR mixes in the
//   inverted brightness for the same reason. Vegetation transpires, so it
//   keeps only a third of the solar gain (cool leaves over warm trunks/soil).
// Bright sand and dark rock converge at midday through the shared ambient and
// soak terms, which is the thermal washout of hot deserts.
float terrain_temp(vec3 albedo_lin, float incidence)
{
    vec3 c = pow(max(albedo_lin, vec3(0.0)), vec3(1.0 / 2.2));
    float lum = dot(c, vec3(0.2126, 0.7152, 0.0722));
    float green = c.g - max(c.r, c.b);
    float veg = smoothstep(0.02, 0.10, green);
    float rock = (1.0 - veg) * (1.0 - smoothstep(0.08, 0.25, max(c.r, max(c.g, c.b)) - min(c.r, min(c.g, c.b))));
    float d_veg = -5.0 + 10.0 * diurnal(0.0);
    float d_soil = -5.0 + 10.0 * diurnal(0.4);
    float d_rock = -7.0 + 14.0 * diurnal(0.0);
    float delta = mix(mix(d_soil, d_rock, rock), d_veg, veg) * clear_sky;
    float soak = (1.0 - lum) * incidence * sun_soak_c * clear_sky * mix(1.0, 0.35, veg);
    return ambient_c + delta + soak;
}

// The block pass marks a known vehicle voxel with a light factor of exactly -1.0
// (no game shader writes a negative light factor); a normalized attachment cannot
// store that, so there the marker is gcolor alpha 0.5 (the game writes 1.0).
bool is_vehicle(vec4 normal, vec4 colour)
{
    return normal_encoded == 1 ? abs(colour.a - 0.5) < 0.02 : normal.w < -0.5;
}

void main()
{
    // G-buffer 0 = gnormal_light_factor, 1 = gcolor (verified: glDrawBuffers is
    // the fixed COLOR_ATTACHMENT0..4 list and opaque declares the outputs in
    // that order).
    // The target may be smaller than the G-buffer (camera feed 128x128 vs
    // 1152x640): sample at the game's own quad texcoords, as its tonemap does.
    vec2 guv = uv;
    vec4 normal = texture(tex_normal, guv);
    vec4 colour = texture(tex_color, guv);

    vec3 n = gbuffer_normal(normal);
    float incidence = max(0.0, dot(n, normalize(sun_dir))) * sun_valid;
    float dist = has_depth == 1 ? decode_linear_depth(texture(tex_depth, guv).r) : -1.0;
    bool sky = has_depth == 1 && dist > sky_depth_m;
    bool marked_any = is_vehicle(normal, colour) || (normal_encoded == 0 && normal.w < -0.5);

    // View ray (render space) for the sea plane and the sun disc.
    vec3 eye = vec3(0.0);
    vec3 ray = vec3(0.0, 0.0, -1.0);
    vec3 fwd = ray;
    if (has_inv == 1)
    {
        vec4 a = mat_inv * vec4(guv * 2.0 - 1.0, -1.0, 1.0);
        vec4 b = mat_inv * vec4(guv * 2.0 - 1.0, 1.0, 1.0);
        vec4 c = mat_inv * vec4(0.0, 0.0, -1.0, 1.0);
        vec4 e = mat_inv * vec4(0.0, 0.0, 1.0, 1.0);
        eye = a.xyz / a.w;
        ray = normalize(b.xyz / b.w - eye);
        fwd = normalize(e.xyz / e.w - c.xyz / c.w);
    }
    // The game's moon/sun sprites sit ~18 km out: unmarked geometry that high
    // above the sea is sky, not terrain.
    if (has_inv == 1 && has_depth == 1 && !sky && !marked_any
        && eye.y + ray.y * dist / max(dot(ray, fwd), 1e-3) > water_y + 3000.0)
        sky = true;
    // Water: the ocean is forward-rendered after the G-buffer, so the
    // G-buffer holds the sea floor. IR does not penetrate water: anything
    // past the sea plane (or everything, from below it) is the surface.
    float water = 0.0;
    if (has_inv == 1)
    {
        if (eye.y < water_y)
            water = 1.0;
        else if (ray.y < -1e-4)
        {
            float t = (water_y - eye.y) / ray.y;
            float plane_depth = t * dot(ray, fwd);
            if (sky || has_depth == 0 || dist > plane_depth * 1.006 + 0.2)
                water = 1.0;
        }
        // The game's own test (light.glslf fog): the rendered ocean surface,
        // waves included, in front of the scene. Same encoding as gdepth.
        if (has_water == 1 && eye.y >= water_y && textureSize(tex_water, 0).x > 32)
        {
            // Where no ocean was drawn the texture holds its clear value
            // (0 = "depth 0", which would read as water in front of
            // everything, sky included): only trust drawn texels, and leave
            // sky pixels to the plane test.
            float raw = texture(tex_water, guv).r;
            if (!sky && raw > 1e-6 && raw < 0.999)
                water = decode_linear_depth(raw) < dist ? 1.0 : 0.0;
        }
        // Unmarked geometry at the waterline (surf foam/bubble particles,
        // wet sand) flickers against the plane test: it reads as water.
        if (water < 0.5 && !marked_any && !sky && has_depth == 1 && dist < 300.0)
        {
            float py = eye.y + ray.y * dist / max(dot(ray, fwd), 1e-3);
            if (py < water_y + 0.4)
                water = 1.0;
        }
    }

    float temp;
    float sun_glow = 0.0; // display-space halo (camera bloom), 0..1
    if (water > 0.5)
    {
        // FlightGear water cycle (-1..+1, lag 0.9) a little under the air,
        // reflecting the cold sky at grazing angles (Fresnel).
        float t_water = ambient_c - 2.0 + (-1.0 + 2.0 * diurnal(0.9)) * clear_sky;
        float fres = eye.y < water_y ? 0.0 : 0.02 + 0.98 * pow(1.0 - abs(ray.y), 5.0);
        temp = mix(t_water, sky_c, fres * 0.35);
    }
    else if (normal_encoded == 0 && normal.w < -4.5 && normal.w > -5.5)
    {
        // Gas particle (exhaust/smoke/steam) from the gas pass.
        temp = vehicle_air_c + colour.r * 1000.0;
    }
    else if (normal_encoded == 0 && normal.w < -2.5 && normal.w > -3.5)
    {
        // Skinned draw (person/animal), marked via its light_factor: clothed
        // skin sits a little above the air, never below ~25 C.
        temp = max(mix(ambient_c, 33.0, 0.7), 25.0);
    }
    else if (is_vehicle(normal, colour))
    {
        // Simulated block temperature plus the painted skin's own sun gain
        // (thin paint/skin heats in minutes, the block mass in hours): dark
        // paint on the sunlit side of a hull reads warmer. colour.g carries
        // the block's paint brightness (block pass).
        // The surface skin (paint over thin plate) settles at the same sun soak
        // as terrain within minutes; the block's bulk temperature (mass-
        // weighted, slow) only shows once it is hotter than that skin.
        float absorb = 1.0 - 0.8 * clamp(colour.g, 0.0, 1.0);
        float bulk = enc_min + colour.r * (enc_max - enc_min);
        temp = max(bulk, vehicle_air_c + absorb * incidence * clear_sky * sun_soak_c);
    }
    else
    {
        if (sky)
        {
            temp = sky_c;
            if (has_inv == 1 && sun_valid > 0.5)
            {
                // Sun along the light pass's own direction: ~2.5 deg saturated
                // core (the game's bright core) plus a sensor-bloom halo that
                // falls off over ~8 deg.
                // Drawn sprite sits below the light direction (sun.zig
                // SUN_SPRITE_DROP, measured in game).
                vec3 disc = normalize(normalize(sun_dir) - vec3(0.0, 0.07, 0.0));
                float ang = acos(clamp(dot(ray, disc), -1.0, 1.0));
                float core = 1.0 - smoothstep(0.036, 0.048, ang);
                temp = mix(temp, 1500.0, core * clear_sky);
                sun_glow = (0.35 * exp(-max(ang - 0.04, 0.0) / 0.035)
                    + 0.10 * exp(-max(ang - 0.04, 0.0) / 0.14)) * clear_sky;
            }
        }
        else
        {
            temp = terrain_temp(colour.rgb, incidence);
        }
    }

    // Jet exhaust: hot gas cones along each nozzle's flow axis, in front of
    // the scene, cooling and widening downstream.
    if (has_inv == 1)
    {
        float scene = (has_depth == 1 && !sky) ? dist : 1e9;
        for (int i = 0; i < plume_n; ++i)
        {
            vec3 p0 = plume_a[i].xyz;
            vec3 d = plume_b[i].xyz;
            float excess = plume_a[i].w;
            float r0 = plume_b[i].w;
            float len = 3.0 + max(excess - 100.0, 0.0) / 100.0;
            vec3 w = eye - p0;
            float b = dot(ray, d);
            float den = max(1.0 - b * b, 1e-4);
            float ta = clamp((dot(d, w) - b * dot(ray, w)) / den, 0.0, len);
            float s = dot(p0 + d * ta - eye, ray);
            if (s <= 0.0 || s * dot(ray, fwd) > scene)
                continue;
            float r = r0 + 0.08 * ta;
            float q = length(eye + ray * s - (p0 + d * ta)) / r;
            if (q < 1.0)
                temp = max(temp, vehicle_air_c + excess * exp(-3.0 * ta / len) * (1.0 - q * q));
        }
    }

    float v = clamp((temp - tmin) / max(tmax - tmin, 0.001), 0.0, 1.0);
    // Keep some surface detail under the temperature paint (sensor contrast
    // from emissivity and shape): paint/albedo brightness and a soft
    // view-facing shade. People and blocks of one temperature stay readable.
    if (water < 0.5 && !sky && !(normal_encoded == 0 && normal.w < -4.5 && normal.w > -5.5))
    {
        float lum;
        if (is_vehicle(normal, colour))
            lum = clamp(colour.g, 0.0, 1.0);
        else
            lum = dot(pow(clamp(colour.rgb, 0.0, 1.0), vec3(1.0 / 2.2)), vec3(0.299, 0.587, 0.114));
        float shade = has_inv == 1 ? max(dot(n, -ray), 0.0) : 0.5 + 0.5 * n.y;
        // Scaled by the signal: below the window stays black, so albedo
        // (painted/lit-looking surfaces) cannot show through on its own.
        v = clamp(v + DETAIL * ((lum - 0.5) * 0.6 + (shade - 0.6)) * smoothstep(0.0, 0.25, v), 0.0, 1.0);
    }
    v = max(v, sun_glow);
    // Black hot = colour inversion of the tinted white-hot picture (like an
    // image editor's invert), not a flip of the brightness inside the tint.
    vec3 rgb = v * tint;
    color_out = vec4(black_hot == 1 ? vec3(1.0) - rgb : rgb, 1.0);
}
