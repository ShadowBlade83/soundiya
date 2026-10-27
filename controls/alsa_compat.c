/* Unversioned references bind to the obsolete ALSA_0.9 ABI on Mini 1.
 * The __snd_ entry points use the current ABI on both station generations.
 * Linker wrapping leaves the vendor library's own calls untouched. */
typedef struct _snd_pcm snd_pcm_t;
typedef struct _snd_pcm_hw_params snd_pcm_hw_params_t;
typedef unsigned long snd_pcm_uframes_t;

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
