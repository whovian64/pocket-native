#include "overrides.h"

/* a copy of the library's own, which Android's loader needs: a call to
   the C library's memcpy is left unresolved there and the whole library
   with it. eight bytes a step, kept a loop rather than made a memcpy call */
__attribute__((optimize("no-tree-loop-distribute-patterns")))
static void copy(uint8_t *dst, const uint8_t *src, uint32_t size) {
    for (; size >= 8; size -= 8, dst += 8, src += 8) {
        uint64_t word;
        __builtin_memcpy(&word, src, 8);
        __builtin_memcpy(dst, &word, 8);
    }
    for (; size; size--)
        *dst++ = *src++;
}

/* the runtime's memcpy, (destination, source, size), done natively: the
   game's hottest function in a race. it leaves the destination and the
   source past what it copied, the rest its callers don't count on.
   regions that overlap go through the game's own, whose 32-byte blocks
   a native copy doesn't repeat */
RECOMP_OVERRIDE(0x0010EE08) {
    uint32_t dst = ctx->r[0], src = ctx->r[1], size = ctx->r[2];
    if (dst - src < size || src - dst < size) {
        CALL(RECOMP_ORIGINAL(0x0010EE08));
        return;
    }
#ifdef RECOMP_FASTMEM
    copy(RECOMP_VIEW(ctx) + dst, RECOMP_VIEW(ctx) + src, size);
#else
    for (uint32_t i = 0; i < size; i++)
        mem_write8(ctx, dst + i, mem_read8(ctx, src + i));
#endif
    ctx->r[0] = dst + size;
    ctx->r[1] = src + size;
    RETURN_TO(ctx->r[14]);
}
