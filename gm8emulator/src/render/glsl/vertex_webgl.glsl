#version 300 es
precision highp float;

struct Light {
    vec4 pos; // padded vec3
    vec4 colour; // padded vec3
    bool enabled;
    bool is_point;
    float range;
};

layout(std140) uniform RenderState {
    // vertex shader
    mat4 model;
    mat4 viewproj;
    Light lights[8];
    vec4 ambient_colour; // padded vec3
    bool lighting_enabled;
    bool gouraud_shading;
    bool colour_blending;
    // frag shader
    bool repeat;
    bool lerp; // only used if repeat is on
    bool alpha_test;
    bool fog_enabled;
    float fog_begin;
    float fog_end;
    vec4 fog_colour; // padded vec3
    mat4 view; // only for texcoord_source; `viewproj` is what actually positions a vertex
    highp int texcoord_source; // 0 vertex, 1 cs normal, 2 cs position, 3 cs reflect, 4 sphere map
};

uniform bool gm81_normalize; // this will never change so we don't need to include it in the state

layout (location = 0) in vec3 pos;
layout (location = 1) in vec4 blend;
layout (location = 2) in vec2 tex_coord;
layout (location = 3) in vec3 normal;
layout (location = 4) in vec4 atlas_xywh;

out vec2 frag_tex_coord;
out vec4 frag_atlas_xywh;
out vec4 frag_blend;
flat out vec4 frag_blend_flat;
out float fog_z;

// D3D9's fixed-function texture coordinate generators, which GM8.2's `texture_set_mode` selects by
// writing D3DTSS_TEXCOORDINDEX. Everything here is in *camera* space, hence the separate `view`.
//
// The normal goes through `model` alone, with no inverse transpose, because that is what this
// renderer's lighting above already does and what GM8 itself does. Normalising is not optional
// though: the sphere-map formula only lands inside 0..1 for a unit normal, and a game scaling a
// model non-uniformly (renex's teapot grows in on two axes at once) would otherwise walk its
// coordinates straight off the texture.
vec2 generated_tex_coord(vec4 world_pos) {
    vec3 cs_normal = normalize((view * (model * vec4(normal, 0.0))).xyz);
    vec3 cs_pos = (view * world_pos).xyz;
    if (texcoord_source == 1) return cs_normal.xy;
    if (texcoord_source == 2) return cs_pos.xy;
    if (texcoord_source == 3) return reflect(normalize(cs_pos), cs_normal).xy;
    // Sphere map, per the formula Direct3D's own documentation gives for it:
    // u = Nx/2 + 0.5, v = Ny/2 + 0.5, on the camera-space normal.
    return cs_normal.xy * 0.5 + 0.5;
}

void main() {
    vec4 world_pos = model * vec4(pos, 1.0);
    frag_atlas_xywh = atlas_xywh;
    frag_blend_flat = vec4(1.0);

    if (any(isnan(tex_coord))) {
        // NaN is how this program is told the draw has no texture at all, so there is nothing for a
        // generator to address either -- it stays untextured whatever `texcoord_source` says.
        frag_tex_coord = vec2(0.0);
        frag_blend = blend;
    } else {
        frag_tex_coord = texcoord_source == 0 ? tex_coord : generated_tex_coord(world_pos);
        frag_blend = colour_blending ? blend : vec4(1.0);
    }

    if (lighting_enabled) {
        vec3 light_col = vec3(0.0);
        vec3 new_normal = -(model * vec4(normal, 0.0)).xyz;
        if (gm81_normalize) {
            new_normal = normalize(new_normal);
        }
        for (int i = 0; i < 8; i++) {
            if (lights[i].enabled) {
                vec3 this_light_col = lights[i].colour.rgb;
                vec3 ray = lights[i].pos.xyz;
                if (lights[i].is_point) {
                    ray = world_pos.xyz - ray;
                    float dist = length(ray);
                    if (dist < lights[i].range) {
                        this_light_col /= 1.0 + (4.0 / lights[i].range) * dist;
                    } else {
                        this_light_col = vec3(0.0);
                    }
                }
                light_col += this_light_col * clamp(dot(normalize(ray), new_normal), 0.0, 1.0);
            }
        }
        if (gouraud_shading) {
            frag_blend.rgb *= light_col;
            frag_blend.rgb += ambient_colour.rgb;
        } else {
            frag_blend_flat.rgb *= light_col;
            frag_blend_flat.rgb += ambient_colour.rgb;
        }
    }
    gl_Position = viewproj * world_pos;
    fog_z = gl_Position.z;
}
