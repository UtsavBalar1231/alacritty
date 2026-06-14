#if defined(GLES2_RENDERER)
#define float_t mediump float
#define color_t mediump vec4
#define FRAG_COLOR gl_FragColor

varying mediump vec2 vTexCoord;
#else
#define float_t float
#define color_t vec4

in vec2 vTexCoord;

out vec4 FragColor;
#define FRAG_COLOR FragColor
#endif

uniform sampler2D image;

void main() {
    FRAG_COLOR = texture2D(image, vTexCoord);
}
