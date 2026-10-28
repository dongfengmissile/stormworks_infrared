#version 330 core
in vec3 vertex_position_in;
in vec3 vertex_normal_in;
in vec3 vertex_color_in;
uniform int has_color;

uniform mat4 mat_view_proj;
uniform mat4 mat_world;
// Chunk draws: identity. Component draws: inverse(body mat) * component mat, so
// flir_pos/flir_lnormal are body-local for the per-fragment voxel lookup.
uniform mat4 comp_to_body;

out vec3 flir_normal;
out vec3 flir_pos;
out vec3 flir_lnormal;
out float log_z;
out float flir_paint;

void main()
{
    // Paint brightness (vertex colour, display space) for the skin sun gain.
    flir_paint = has_color == 1 ? dot(clamp(vertex_color_in, 0.0, 1.0), vec3(0.2126, 0.7152, 0.0722)) : 0.5;
    // The game's own opaque VS writes the WORLD normal into the G-buffer
    // (`(mat_world * vec4(vertex_normal_in, 0.0)).xyz`), so the substitute has
    // to write the same space: attachment 0 is a world-space normal.
    flir_normal = (mat_world * vec4(vertex_normal_in, 0.0)).xyz;
    flir_pos = (comp_to_body * vec4(vertex_position_in, 1.0)).xyz;
    flir_lnormal = mat3(comp_to_body) * vertex_normal_in;
    gl_Position = mat_view_proj * (mat_world * vec4(vertex_position_in, 1.0));
    // depth_utils.glslh (USE_LOG2 + DEPTH_LOG_IN_FRAGMENT): the game writes log
    // depth, so our substitute draw must write the identical value or the
    // depth buffer the later deferred passes read would be corrupted.
    log_z = gl_Position.w * 0.01 + 1.0;
}
