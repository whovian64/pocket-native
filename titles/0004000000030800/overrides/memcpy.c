#include "overrides.h"

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
    __builtin_memcpy(RECOMP_VIEW(ctx) + dst, RECOMP_VIEW(ctx) + src, size);
#else
    for (uint32_t i = 0; i < size; i++)
        mem_write8(ctx, dst + i, mem_read8(ctx, src + i));
#endif
    ctx->r[0] = dst + size;
    ctx->r[1] = src + size;
    RETURN_TO(ctx->r[14]);
}
