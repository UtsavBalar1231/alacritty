#if defined(GLES2_RENDERER)
attribute vec2 aPosition;
attribute vec2 aTexCoord;

varying mediump vec2 vTexCoord;
#else
layout (location = 0) in vec2 aPosition;
layout (location = 1) in vec2 aTexCoord;

out vec2 vTexCoord;
#endif

// projection.xy = NDC offset, projection.zw = NDC scale (pixels → NDC).
uniform vec4 projection;

void main() {
    vTexCoord = aTexCoord;
    vec2 ndc = projection.xy + aPosition * projection.zw;
    gl_Position = vec4(ndc, 0.0, 1.0);
}
