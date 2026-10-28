
in float log_z;
in vec2 uv;
in vec3 puff;
in vec3 col;
uniform sampler2D texture_noise0;
uniform float timer;
uniform float air_c; // the composite adds the excess to this air temperature
out vec4 flir_out0;
out vec4 flir_out1;
out vec4 flir_out2;
void main()
{
    gl_FragDepth = log2(max(log_z, 1e-6)) / 7.658227526203386;
    float excess;
#if KIND == 0
    if (texture(texture_noise0, uv + puff.xy).r > puff.z * 6.0 * (0.5 - length(uv)))
        discard;
    float life = clamp(puff.z, 0.0, 1.0);          // remaining life
    float lum = (col.r + col.g + col.b) / 3.0;
    if (col.r > 0.8 && col.g < 0.4)
        excess = 900.0;                            // lava splash
    else if (col.r > col.g + 0.02)
        excess = max(37.0 - air_c, 0.0);           // blood: body temperature
    else if (lum > 0.4)
        excess = 8.0;                              // water spray / steam
    else
        excess = 10.0 + 390.0 * pow(life, 6.0);       // exhaust / smoke: hot at the outlet, cooling as it drifts
#elif KIND == 1 || KIND == 3
    float nf = texture(texture_noise0, uv + vec2(0.0, timer) * 0.2 + puff.yz).r
             * texture(texture_noise0, uv * 1.4 + vec2(timer, 0.0) * 0.2 + puff.yz).r;
    float df = clamp(2.5 - 2.0 * clamp(length(uv - vec2(0.5)) * 2.0, 0.0, 1.0), 0.0, 1.0);
    if (nf * df <= mix(0.2, 0.6, 1.0 - puff.x))
        discard;
#if KIND == 1
    float heat = 1.0 - clamp((1.0 - puff.x) * 3.0, 0.0, 1.0);
    excess = mix(300.0, 1000.0, heat);             // burning flare / flame
#else
    float life = clamp(puff.x, 0.0, 1.0);          // remaining life (-1: none)
    excess = 10.0 + 490.0 * pow(life, 6.0);           // plume: hot at the nozzle, cools downstream
#endif
#else
    float nf = texture(texture_noise0, uv + vec2(0.0, timer) * 0.5 + puff.yz).r
             * texture(texture_noise0, uv * 1.4 + puff.yz).r;
    float dc = clamp(1.0 - length(uv - vec2(0.5)), 0.0, 1.0);
    float life = clamp(puff.x, 0.0, 1.0);
    if (nf * dc < 0.25 + (1.0 - pow(life, 0.1)) * 0.75)
        discard;
    excess = 300.0 + 600.0 * life;                 // fireball
#endif
    flir_out0 = vec4(0.0, 1.0, 0.0, -5.0);
    flir_out1 = vec4(min(excess / 1000.0, 1.0), 0.0, 0.0, 1.0);
    flir_out2 = vec4(0.0);
}
