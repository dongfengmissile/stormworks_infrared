#version 330 core
in vec3 vertex_position_in;
in vec2 vertex_coord0_in;
out vec2 uv;
uniform mat4 mat_view_proj;
void main()
{
    uv = vertex_coord0_in;
    gl_Position = mat_view_proj * vec4(vertex_position_in, 1.0);
}
