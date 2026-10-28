#version 330 core
in vec3 flir_normal;
in vec3 flir_pos;
in vec3 flir_lnormal;
in float log_z;
in float flir_paint;

uniform ivec3 grid_min;
uniform ivec3 grid_dims;
uniform float enc_min;
uniform float enc_max;
uniform sampler3D tex_temps;
uniform int normal_encoded;
// Component draws: the component's own origin voxel (body-local); chunk draws
// set has_comp_voxel = 0.
uniform ivec3 comp_voxel;
uniform int has_comp_voxel;

out vec4 flir_out0;
out vec4 flir_out1;
out vec4 flir_out2;

// Temperature of a body-local voxel, EMPTY when empty/outside the grid. Empty
// voxels are NaN by contract (see BodyGrid), and 0.0 is an unwritten texel:
// neither may read as a cold vehicle (no marker -> composite shows ambient).
const float EMPTY = -1000.0;
float cell_temp(ivec3 voxel)
{
    ivec3 local = voxel - grid_min;
    if (any(lessThan(local, ivec3(0))) || any(greaterThanEqual(local, grid_dims)))
        return EMPTY;
    float t = texelFetch(tex_temps, local, 0).r;
    return (isnan(t) || t <= -998.0 || t == 0.0) ? EMPTY : t;
}

void main()
{
    // FC = log2(far * C + 1), far 20100, C 0.01
    gl_FragDepth = log2(max(log_z, 1e-6)) / 7.658227526203386;

    // Voxel centres sit at voxel*0.25 body-local and faces at +-0.125, so a
    // surface point moved 0.0625 m back along its normal is inside its voxel.
    // Per fragment (not per vertex) so the paint is square 0.25 m cells.
    ivec3 voxel = ivec3(floor((flir_pos - normalize(flir_lnormal) * 0.0625) / 0.25 + 0.5));
    float t = cell_temp(voxel);
    // Geometry outside its part's listed voxels (a microcontroller's footprint
    // beyond its one definition voxel, mesh overhang): borrow the average of
    // the occupied cells around it, then the component's own origin cell.
    if (t == EMPTY)
    {
        float sum = 0.0;
        float n = 0.0;
        for (int dz = -1; dz <= 1; dz++)
        for (int dy = -1; dy <= 1; dy++)
        for (int dx = -1; dx <= 1; dx++)
        {
            float u = cell_temp(voxel + ivec3(dx, dy, dz));
            if (u != EMPTY) { sum += u; n += 1.0; }
        }
        if (n > 0.0)
            t = sum / n;
        else if (has_comp_voxel == 1)
            t = cell_temp(comp_voxel);
    }
    bool known = t != EMPTY;
    float enc = known ? clamp((t - enc_min) / max(enc_max - enc_min, 1.0), 0.0, 1.0) : 0.0;

    // G-buffer location 0 = gnormal_light_factor_out: the game writes the full
    // WORLD-space normal in xyz and the light factor in .w
    // (`vec4(normalize(vertex_normal_out), light_factor)` in opaque.glslf), so we
    // write the identical packing. The vehicle marker is a light factor of
    // exactly -1.0: no game shader writes a negative light factor, and light.glslf
    // only thresholds .w (< 0.25 -> 0), so it cannot leak into the game's passes.
    vec3 n = normalize(flir_normal);
    if (normal_encoded == 1)
    {
        // Normalized-attachment fallback (the game uses an RGBA16F normal
        // attachment): only xy can be packed, so the marker moves to gcolor alpha
        // (0.5 is exact in RGBA8; the game writes 1.0).
        flir_out0 = vec4(n.xy * 0.5 + 0.5, 1.0, 1.0);
        flir_out1 = vec4(enc, flir_paint, 0.0, known ? 0.5 : 1.0);
    }
    else
    {
        flir_out0 = vec4(n, known ? -1.0 : 1.0);
        flir_out1 = vec4(enc, flir_paint, 0.0, 1.0);
    }
    // G-buffer location 2 = gvelocity_out: a substitute draw has no motion
    // history of its own, and a stale value would make temporal AA reproject
    // the composite with some other pass's velocity (ghosting around vehicles).
    flir_out2 = vec4(0.0);
}
