
in vec3 vertex_position_in;
in vec3 vertex_normal_in;
in vec2 vertex_uv_in;
in vec4 vertex_color_in;
#if KIND == 1 || KIND == 3
in vec4 vertex_bone_weights_in; // flare: previous position (motion blend)
uniform float interp_factor;
#endif
uniform mat4 mat_view_proj;
uniform mat4 mat_world;
out float log_z;
out vec2 uv;
out vec3 puff;
out vec3 col;
void main()
{
    vec3 p = vertex_position_in;
#if KIND == 1 || KIND == 3
    p = mix(vertex_bone_weights_in.xyz, vertex_position_in, interp_factor);
#endif
    uv = vertex_uv_in;
    puff = vertex_normal_in;
#if KIND == 0
    col = vertex_color_in.rgb;
#else
    col = vec3(0.0);
#endif
    gl_Position = mat_view_proj * (mat_world * vec4(p, 1.0));
    log_z = gl_Position.w * 0.01 + 1.0;
}
