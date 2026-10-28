/* Unversioned references bind to the obsolete ALSA_0.9 ABI on Mini 1.
 * The __snd_ entry points use the current ABI on both station generations.
 * Linker wrapping leaves the vendor library's own calls untouched. */
typedef struct _snd_pcm snd_pcm_t;
typedef struct _snd_pcm_hw_params snd_pcm_hw_params_t;
typedef unsigned long snd_pcm_uframes_t;

/* get_format and get_rate exist in two ABIs: the current one stores the value through the
 * pointer and returns 0, the obsolete ALSA_0.9 one returns the value itself. Which one an
 * unversioned reference binds to differs between station models (Mini 1 binds to the old
 * one, the Lite generation to the current one), and the __snd_ entry points are the opposite
 * way round on the two, so call what the linker gave us and sort out the answer. */
extern int __real_snd_pcm_hw_params_get_format(const snd_pcm_hw_params_t *, int *);
int __wrap_snd_pcm_hw_params_get_format(const snd_pcm_hw_params_t *p, int *v) {
    const int unset = -0x5a5a5a5a;
    *v = unset;
    int r = __real_snd_pcm_hw_params_get_format(p, v);
    if (*v == unset && r >= 0) { /* old ABI: the format came back as the return value */
        *v = r;
        return 0;
    }
    return r;
}
extern int __real_snd_pcm_hw_params_get_rate(const snd_pcm_hw_params_t *, unsigned int *, int *);
int __wrap_snd_pcm_hw_params_get_rate(const snd_pcm_hw_params_t *p, unsigned int *v, int *d) {
    *v = 0;
    if (d)
        *d = 0;
    int r = __real_snd_pcm_hw_params_get_rate(p, v, d);
    if (r > 0) { /* old ABI: the rate came back as the return value (and `d` was written over) */
        *v = (unsigned int)r;
        if (d)
            *d = 0;
        return 0;
    }
    return r;
}

#define BUFFER_GETTER(name) \
    extern int __snd_pcm_hw_params_##name(const snd_pcm_hw_params_t *, snd_pcm_uframes_t *); \
    int __wrap_snd_pcm_hw_params_##name(const snd_pcm_hw_params_t *p, snd_pcm_uframes_t *v) { \
        return __snd_pcm_hw_params_##name(p, v); \
    }

#define PERIOD_GETTER(name) \
    extern int __snd_pcm_hw_params_##name(const snd_pcm_hw_params_t *, snd_pcm_uframes_t *, int *); \
    int __wrap_snd_pcm_hw_params_##name(const snd_pcm_hw_params_t *p, snd_pcm_uframes_t *v, int *d) { \
        return __snd_pcm_hw_params_##name(p, v, d); \
    }

BUFFER_GETTER(get_buffer_size)
BUFFER_GETTER(get_buffer_size_min)
BUFFER_GETTER(get_buffer_size_max)
PERIOD_GETTER(get_period_size)
PERIOD_GETTER(get_period_size_min)
PERIOD_GETTER(get_period_size_max)

extern int __snd_pcm_hw_params_set_buffer_size_near(snd_pcm_t *, snd_pcm_hw_params_t *, snd_pcm_uframes_t *);
int __wrap_snd_pcm_hw_params_set_buffer_size_near(snd_pcm_t *pcm, snd_pcm_hw_params_t *p, snd_pcm_uframes_t *v) {
    return __snd_pcm_hw_params_set_buffer_size_near(pcm, p, v);
}

extern int __snd_pcm_hw_params_set_period_size_near(snd_pcm_t *, snd_pcm_hw_params_t *, snd_pcm_uframes_t *, int *);
int __wrap_snd_pcm_hw_params_set_period_size_near(snd_pcm_t *pcm, snd_pcm_hw_params_t *p, snd_pcm_uframes_t *v, int *d) {
    return __snd_pcm_hw_params_set_period_size_near(pcm, p, v, d);
}
