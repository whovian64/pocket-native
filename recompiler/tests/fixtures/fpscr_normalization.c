#include "recomp.h"
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>

static uint64_t executions;

/* The reference uses IEEE fields, not the implementation's unsigned range.
   This includes the first NaN after infinity and the last subnormal before
   the minimum normal, so widening either fast-path boundary is observable. */
static uint32_t oracle_single(uint32_t bits, uint32_t control) {
    uint32_t exponent = (bits >> 23) & 255u;
    uint32_t fraction = bits & 0x007FFFFFu;
    if (exponent == 0 && fraction != 0 && (control & FPSCR_FZ))
        return bits & 0x80000000u;
    if (exponent == 255u && fraction != 0 && (control & FPSCR_DN))
        return 0x7FC00000u;
    return bits;
}

static uint64_t oracle_double(uint64_t bits, uint32_t control) {
    uint32_t exponent = (uint32_t)((bits >> 52) & 2047u);
    uint64_t fraction = bits & 0x000FFFFFFFFFFFFFull;
    if (exponent == 0 && fraction != 0 && (control & FPSCR_FZ))
        return bits & 0x8000000000000000ull;
    if (exponent == 2047u && fraction != 0 && (control & FPSCR_DN))
        return 0x7FF8000000000000ull;
    return bits;
}

static void mismatch(const char *what, uint64_t input, uint32_t control,
                     uint64_t expected, uint64_t actual) {
    fprintf(stderr, "%s input=%016" PRIx64 " FPSCR=%08" PRIx32
            " expected=%016" PRIx64 " actual=%016" PRIx64 "\n",
            what, input, control, expected, actual);
    exit(1);
}

static uint32_t canary(int index) {
    return 0xA55A0000u ^ (uint32_t)index;
}

static void check_single(uint32_t input, uint32_t mode, int macro) {
    uint32_t words[32], control = mode;
    for (int i = 0; i < 32; i++) words[i] = canary(i);
    Context ctx = {0};
    ctx.vfp = words;
    ctx.fpscr = &control;
    union { uint32_t bits; float value; } value = {.bits = input};
    if (macro) vfp_set_s(&ctx, 7, value.value);
    else (vfp_set_s)(&ctx, 7, value.value);
    uint32_t expected = oracle_single(input, mode);
    for (int i = 0; i < 32; i++) {
        uint32_t wanted = i == 7 ? expected : canary(i);
        if (words[i] != wanted)
            mismatch(macro ? "single macro/register" : "single inline/register",
                     input, mode, wanted, words[i]);
    }
    if (control != mode) mismatch("single FPSCR", input, mode, mode, control);
    executions++;
}

static void check_double(uint64_t input, uint32_t mode) {
    uint32_t words[32], control = mode;
    for (int i = 0; i < 32; i++) words[i] = canary(i);
    Context ctx = {0};
    ctx.vfp = words;
    ctx.fpscr = &control;
    union { uint64_t bits; double value; } value = {.bits = input};
    (vfp_set_d)(&ctx, 6, value.value);
    uint64_t expected = oracle_double(input, mode);
    for (int i = 0; i < 32; i++) {
        uint32_t wanted = i == 12 ? (uint32_t)expected :
                          i == 13 ? (uint32_t)(expected >> 32) : canary(i);
        if (words[i] != wanted)
            mismatch("double inline/register", input, mode, wanted, words[i]);
    }
    if (control != mode) mismatch("double FPSCR", input, mode, mode, control);
    executions++;
}

int main(void) {
    /* Both sides of the quiet/signaling payload boundary, plus fraction
       extrema, cover NaNs and the smallest/largest signed subnormals. */
    const uint32_t single_fractions[] = {
        0, 1, 0x003FFFFFu, 0x00400000u, 0x007FFFFEu, 0x007FFFFFu
    };
    const uint64_t double_fractions[] = {
        0, 1, 0x0007FFFFFFFFFFFFull, 0x0008000000000000ull,
        0x000FFFFFFFFFFFFEull, 0x000FFFFFFFFFFFFFull
    };
    /* Unrelated NZCV, rounding, sticky exception, length and stride bits
       must survive normalization, including non-scalar control values. */
    const uint32_t unrelated[] = {0, 0xA0C0009Fu, 0x50B70080u};
    for (uint32_t flags = 0; flags < 4; flags++) {
        for (unsigned extra = 0; extra < sizeof(unrelated) / sizeof(*unrelated); extra++) {
            uint32_t control = unrelated[extra] |
                ((flags & 1) ? FPSCR_FZ : 0) | ((flags & 2) ? FPSCR_DN : 0);
            for (uint32_t sign = 0; sign < 2; sign++) {
                for (uint32_t exponent = 0; exponent < 256; exponent++) {
                    for (unsigned f = 0; f < sizeof(single_fractions) / sizeof(*single_fractions); f++) {
                        uint32_t bits = (sign << 31) | (exponent << 23) | single_fractions[f];
                        check_single(bits, control, 0);
                        check_single(bits, control, 1);
                    }
                }
                for (uint64_t exponent = 0; exponent < 2048; exponent++) {
                    for (unsigned f = 0; f < sizeof(double_fractions) / sizeof(*double_fractions); f++) {
                        uint64_t bits = ((uint64_t)sign << 63) | (exponent << 52) | double_fractions[f];
                        check_double(bits, control);
                    }
                }
            }
        }
    }
    printf("checked %" PRIu64 " C normalization executions\n", executions);
    return 0;
}
