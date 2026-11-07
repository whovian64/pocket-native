#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl31.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <gbm.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <unistd.h>

/* Linux compiler diagnostic, driven by mali_compiler_check.py. The caller must
 * provide a locally built upstream Panfrost NO-OP shim. A draw forces backend
 * compilation; it cannot provide pixels, GPU execution, or timing evidence. */
static void fail(const char *message) {
    fprintf(stderr, "mali compiler check: %s\n", message);
    exit(2);
}

static int same_path(const char *left, const char *right) {
    char a[PATH_MAX], b[PATH_MAX];
    return left && right && realpath(left, a) && realpath(right, b) && !strcmp(a, b);
}

static void verify_shim(void) {
    const char *expected = getenv("MALI_EXPECTED_SHIM");
    const char *gpu = getenv("PAN_GPU_ID");
    Dl_info info;
    void *symbol = dlsym(RTLD_DEFAULT, "open");
    if (!expected || !symbol || !dladdr(symbol, &info) || !same_path(expected, info.dli_fname))
        fail("open() is not intercepted by the explicitly supplied no-op shim");
    if (!gpu || strcmp(gpu, "7212"))
        fail("PAN_GPU_ID must explicitly select hexadecimal 7212 (Mali-G52)");
    fprintf(stderr, "DRM_OPEN_INTERCEPTOR=%s\n", info.dli_fname);
}

static void verify_kernel_render_fd(int fd) {
    /* Mesa interposes libc fstat/readlink to advertise a fake DRM device.
     * Inspect the real kernel descriptor instead, before GBM/EGL can use it.
     * Upstream Mesa 26.2.5 opens /dev/null (character 1:3) for this descriptor.
     * Assurance still depends on using the pinned upstream no-op shim. */
    _Static_assert(sizeof(void *) == 8, "compiler check requires 64-bit Linux");
    struct stat state;
    if (syscall(SYS_fstat, fd, &state) != 0)
        fail("raw kernel fstat failed for render descriptor");
    unsigned int device_major = major(state.st_rdev), device_minor = minor(state.st_rdev);
    if (S_ISCHR(state.st_mode) && device_major == 226)
        fail("real DRM character descriptor rejected before GBM/EGL");
    if (!S_ISCHR(state.st_mode) || device_major != 1 || device_minor != 3)
        fail("kernel render descriptor is not upstream shim's /dev/null character 1:3");
    char proc_path[64], target[PATH_MAX];
    snprintf(proc_path, sizeof(proc_path), "/proc/self/fd/%d", fd);
    ssize_t size = syscall(SYS_readlinkat, AT_FDCWD, proc_path, target, sizeof(target) - 1);
    if (size <= 0 || size >= (ssize_t)sizeof(target) - 1)
        fail("raw kernel render descriptor link unavailable");
    target[size] = 0;
    if (strcmp(target, "/dev/null"))
        fail("kernel render descriptor link is not /dev/null");
    fprintf(stderr,
            "KERNEL_RENDER_FD_TYPE=character\nKERNEL_RENDER_FD_MAJOR=%u\n"
            "KERNEL_RENDER_FD_MINOR=%u\nKERNEL_RENDER_FD_PATH=%s\n",
            device_major, device_minor, target);
}

static void verify_libraries(void) {
    const char *expected = getenv("MALI_EXPECTED_DRIVER");
    int found = 0;
    FILE *maps = fopen("/proc/self/maps", "r");
    if (!maps || !expected)
        fail("expected driver and Linux library maps required");
    char line[4096];
    while (fgets(line, sizeof(line), maps)) {
        char *path = strchr(line, '/');
        if (!path || !strstr(path, ".so"))
            continue;
        path[strcspn(path, "\n")] = 0;
        if (same_path(path, expected))
            found = 1;
        fprintf(stderr, "LOADED_LIBRARY=%s\n", path);
    }
    fclose(maps);
    if (!found)
        fail("the explicitly supplied driver library was not loaded");
}

static char *read_source(const char *path) {
    FILE *f = fopen(path, "rb");
    if (!f) {
        perror(path);
        exit(2);
    }
    if (fseek(f, 0, SEEK_END))
        fail("cannot seek shader source");
    long n = ftell(f);
    if (n <= 0 || n > 16 * 1024 * 1024)
        fail("shader source must be 1 byte to 16 MiB");
    rewind(f);
    char *p = calloc(n + 1, 1);
    if (!p || fread(p, 1, n, f) != (size_t)n)
        exit(2);
    if (memchr(p, 0, n))
        fail("shader source contains an embedded NUL");
    fclose(f);
    return p;
}
static GLuint shader(GLenum stage, const char *path) {
    char *source = read_source(path);
    const char *p = source;
    GLuint s = glCreateShader(stage);
    glShaderSource(s, 1, &p, NULL);
    glCompileShader(s);
    free(source);
    GLint ok = 0;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char msg[16384];
        glGetShaderInfoLog(s, sizeof(msg), NULL, msg);
        fprintf(stderr, "%s: %s\n", path, msg);
        exit(3);
    }
    return s;
}
int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: mali_compile_pair vertex.vert fragment.frag\n");
        return 2;
    }
    verify_shim();
    int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        perror("shim render node");
        return 2;
    }
    verify_kernel_render_fd(fd);
    struct gbm_device *gbm = gbm_create_device(fd);
    if (!gbm) {
        fprintf(stderr, "gbm_create_device failed\n");
        return 2;
    }
    EGLDisplay dpy = eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR, gbm, NULL);
    EGLint major, minor;
    if (!eglInitialize(dpy, &major, &minor)) {
        fprintf(stderr, "eglInitialize 0x%x\n", eglGetError());
        return 2;
    }
    if (!eglBindAPI(EGL_OPENGL_ES_API))
        fail("cannot bind GLES API");
    EGLint ca[] = {EGL_RENDERABLE_TYPE,
                   EGL_OPENGL_ES3_BIT,
                   EGL_SURFACE_TYPE,
                   EGL_WINDOW_BIT,
                   EGL_RED_SIZE,
                   8,
                   EGL_GREEN_SIZE,
                   8,
                   EGL_BLUE_SIZE,
                   8,
                   EGL_ALPHA_SIZE,
                   8,
                   EGL_NONE};
    EGLConfig cfg;
    EGLint count;
    if (!eglChooseConfig(dpy, ca, &cfg, 1, &count) || !count)
        return 2;
    EGLint ctxa[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 1, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctxa);
    if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
        fprintf(stderr, "context creation 0x%x\n", eglGetError());
        return 2;
    }
    const char *vendor = (const char *)glGetString(GL_VENDOR);
    const char *renderer = (const char *)glGetString(GL_RENDERER);
    const char *version = (const char *)glGetString(GL_VERSION);
    fprintf(stderr, "GL_VENDOR=%s\nGL_RENDERER=%s\nGL_VERSION=%s\n", vendor, renderer, version);
    if (!vendor || strcmp(vendor, "Mesa") || !renderer || strncmp(renderer, "Mali-G52 ", 9) ||
        !strstr(renderer, "(Panfrost)") || !version || !strstr(version, "Mesa"))
        fail("active context must be Mesa Panfrost Mali-G52");
    verify_libraries();
    GLuint vs = shader(GL_VERTEX_SHADER, argv[1]), fs = shader(GL_FRAGMENT_SHADER, argv[2]);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    GLint ok;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char msg[16384];
        glGetProgramInfoLog(prog, sizeof(msg), NULL, msg);
        fprintf(stderr, "link: %s\n", msg);
        return 4;
    }
    glUseProgram(prog);
    GLint blocks = 0;
    glGetProgramiv(prog, GL_ACTIVE_UNIFORM_BLOCKS, &blocks);
    for (GLint i = 0; i < blocks; ++i) {
        GLint size, binding;
        glGetActiveUniformBlockiv(prog, i, GL_UNIFORM_BLOCK_DATA_SIZE, &size);
        glGetActiveUniformBlockiv(prog, i, GL_UNIFORM_BLOCK_BINDING, &binding);
        GLuint buf;
        glGenBuffers(1, &buf);
        glBindBuffer(GL_UNIFORM_BUFFER, buf);
        void *zero = calloc(size ? size : 16, 1);
        if (!zero)
            fail("uniform allocation failed");
        glBufferData(GL_UNIFORM_BUFFER, size ? size : 16, zero, GL_STATIC_DRAW);
        free(zero);
        glBindBufferBase(GL_UNIFORM_BUFFER, binding, buf);
    }
    glGetProgramInterfaceiv(prog, GL_SHADER_STORAGE_BLOCK, GL_ACTIVE_RESOURCES, &blocks);
    for (GLint i = 0; i < blocks; ++i) {
        GLenum props[] = {GL_BUFFER_BINDING, GL_BUFFER_DATA_SIZE};
        GLint values[2];
        glGetProgramResourceiv(prog, GL_SHADER_STORAGE_BLOCK, i, 2, props, 2, NULL, values);
        GLsizeiptr size = values[1] > 65536 ? values[1] : 65536;
        GLuint buf;
        glGenBuffers(1, &buf);
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, buf);
        void *zero = calloc(size, 1);
        if (!zero)
            fail("storage allocation failed");
        glBufferData(GL_SHADER_STORAGE_BUFFER, size, zero, GL_STATIC_DRAW);
        free(zero);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, values[0], buf);
    }
    GLuint tex, fbo, vao;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, 4, 4);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 5;
    for (int unit = 0; unit < 6; ++unit) {
        GLuint sampled;
        glActiveTexture(GL_TEXTURE0 + unit);
        glGenTextures(1, &sampled);
        glBindTexture(GL_TEXTURE_2D, sampled);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, 4, 4);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    }
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glViewport(0, 0, 4, 4);
    if (glGetError() != GL_NO_ERROR)
        fail("diagnostic resource setup failed");
    glDrawArrays(GL_TRIANGLES, 0, 3);
    GLenum err = glGetError();
    fprintf(stderr, "NOOP_DRAW_GL_ERROR=0x%x\n", err);
    fprintf(stderr, "NO_GPU_EXECUTION=1\n");
    /* No finish/readback: driver compilation is the only evidence sought. */
    return err == GL_NO_ERROR ? 0 : 5;
}
