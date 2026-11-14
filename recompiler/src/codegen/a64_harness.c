/* runs a generated library's code from random contexts through the host's
   resume loop and prints one digest per trial, for comparing the C alone
   with the C and the assembly. memory is a view of 4 GiB whose pages come
   on first use, filled from their address, as the host's fastmem maps
   them. */

#define _GNU_SOURCE
#include "recomp.h"
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>

extern const uint32_t recomp_entry_count;
extern const Entry recomp_entries[];

#define SPAN ((uint64_t)1 << 32)
#define SENTINEL 0x7F000000u
#define MOST_PAGES 65536

static uint8_t *view;
static uint32_t pages[MOST_PAGES];
static int page_count;
static uint64_t trace;

static uint64_t mix(uint64_t x) {
    x += 0x9E3779B97F4A7C15ull;
    x ^= x >> 30;
    x *= 0xBF58476D1CE4E5B9ull;
    x ^= x >> 27;
    x *= 0x94D049BB133111EBull;
    return x ^ (x >> 31);
}

static void fold(uint64_t value) {
    trace = mix(trace ^ value);
}

static void on_fault(int signal_number, siginfo_t *info, void *context) {
    (void)signal_number;
    (void)context;
    uintptr_t at = (uintptr_t)info->si_addr;
    if (at < (uintptr_t)view || at >= (uintptr_t)view + SPAN + 4096) {
        fprintf(stderr, "fault outside the view at %p, the view at %p\n", (void *)at, (void *)view);
        _exit(3);
    }
    if (page_count >= MOST_PAGES) {
        fprintf(stderr, "more than %d pages\n", MOST_PAGES);
        _exit(4);
    }
    uintptr_t page = at & ~(uintptr_t)4095;
    mprotect((void *)page, 4096, PROT_READ | PROT_WRITE);
    uint64_t offset = page - (uintptr_t)view;
    for (int i = 0; i < 512; i++) ((uint64_t *)page)[i] = mix(offset + (uint64_t)i);
    pages[page_count++] = (uint32_t)(offset >> 12);
}

static int by_number(const void *a, const void *b) {
    uint32_t x = *(const uint32_t *)a, y = *(const uint32_t *)b;
    return (x > y) - (x < y);
}

static Code lookup(Context *ctx, uint32_t address) {
    (void)ctx;
    uint32_t low = 0, high = recomp_entry_count;
    while (low < high) {
        uint32_t middle = (low + high) / 2;
        if (recomp_entries[middle].address < address) low = middle + 1;
        else high = middle;
    }
    return low < recomp_entry_count && recomp_entries[low].address == address ? recomp_entries[low].code : 0;
}

static void interpret(Context *ctx, uint32_t address, uint32_t opcode) {
    fold(0x1000000000ull | address);
    ctx->r[0] = ctx->r[0] * 31u + opcode;
}

static void svc(Context *ctx, uint32_t number) {
    fold(0x2000000000ull | number);
    ctx->r[0] ^= number;
}

static const Host host = { .interpret = interpret, .lookup = lookup, .svc = svc };

int main(int argc, char **argv) {
    int trials = argc > 1 ? atoi(argv[1]) : 100;
    uint32_t entry = argc > 2 ? (uint32_t)strtoul(argv[2], 0, 16) : 0x00100000u;
    /* a trial to trace, every resume printed */
    int traced = argc > 3 ? atoi(argv[3]) : -1;
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_sigaction = on_fault;
    action.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &action, 0);
    static uint32_t vfp[32], fpscr;
    for (int t = 0; t < trials; t++) {
        view = mmap(0, SPAN + 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
        if (view == MAP_FAILED) {
            perror("mmap");
            return 2;
        }
        page_count = 0;
        trace = 0;
        Context ctx;
        memset(&ctx, 0, sizeof ctx);
        uint64_t s = mix((uint64_t)t * 7919u + 1);
        for (int i = 0; i < 11; i++) {
            s = mix(s);
            /* half full width, the rest small, half of those the shift
               amounts around 32 that change how shifts by a register go */
            ctx.r[i] = i % 2 == 0 ? (uint32_t)s : (s >> 40) & 1 ? (uint32_t)(s & 0x1FF) : (uint32_t)(s % 41);
        }
        s = mix(s);
        ctx.r[11] = (uint32_t)(s & 0x3C);
        ctx.r[12] = 0x08000000u + (uint32_t)(mix(s) & 0xFFC);
        ctx.r[13] = 0x08100000u;
        ctx.r[14] = SENTINEL;
        ctx.r[15] = entry;
        ctx.tls = (uint32_t)mix(s + 3);
        s = mix(s);
        ctx.n = s & 1;
        ctx.z = (s >> 1) & 1;
        ctx.c = (s >> 2) & 1;
        ctx.v = (s >> 3) & 1;
        ctx.budget = 1 + (int32_t)(mix(s) % 3000);
        /* VFP registers mostly ordinary numbers, now and then a zero, a
           subnormal or anything at all; fpscr mostly flushing to zero with
           default NaNs as games run, now and then not, now and then a
           short vector */
        for (int i = 0; i < 32; i++) {
            s = mix(s);
            uint32_t kind = (uint32_t)(s >> 60);
            if (kind < 10) vfp[i] = ((uint32_t)s & 0x807FFFFFu) | ((0x70u + (uint32_t)(s >> 40) % 24) << 23);
            else if (kind < 12) vfp[i] = (uint32_t)s & 0x80000000u;
            else if (kind < 14) vfp[i] = (uint32_t)s & 0x807FFFFFu;
            else vfp[i] = (uint32_t)s;
        }
        s = mix(s);
        fpscr = (s & 7) < 5 ? 0x03000000u : (uint32_t)(s >> 8) & 0xF3000000u;
        if ((s >> 4) % 8 == 0) fpscr |= 1u << 16;
        __asm__ volatile("msr fpcr, %0" : : "r"((uint64_t)(fpscr & 0x03000000u)) : "memory");
        ctx.fastmem = view;
        ctx.vfp = vfp;
        ctx.fpscr = &fpscr;
        ctx.host = &host;
        int resumes;
        /* about as many instructions as a frame's worth, past which a loop
           walking memory would map it all */
        int64_t ran = 0;
        for (resumes = 0; resumes < 4000 && ran < 400000; resumes++) {
            if (!ctx.thumb && ctx.r[15] == SENTINEL) break;
            Code code = lookup(&ctx, ctx.r[15] | ctx.thumb);
            if (!code) {
                fold(0xDEAD);
                break;
            }
            ctx.exit = 0;
            int32_t given = ctx.budget;
            code(&ctx);
            ran += given - ctx.budget;
            /* the host counts cycles from what is left of the budget */
            fold(((uint64_t)ctx.exit << 32) | ctx.r[15]);
            fold((uint32_t)ctx.budget);
            if (t == traced) {
                printf("%d exit %u r15 %08x", resumes, ctx.exit, ctx.r[15]);
                for (int i = 0; i < 15; i++) printf(" %08x", ctx.r[i]);
                printf(" nzcv %u%u%u%u budget %d depth %u\n", ctx.n, ctx.z, ctx.c, ctx.v, ctx.budget, ctx.depth);
            }
            /* now and then a little, so that the budget runs out in odd
               places, mostly as much as the host gives, which a long block
               needs to start at all */
            if (ctx.exit == EXIT_BUDGET) ctx.budget = resumes % 3 == 0 ? 1 + resumes % 97 : 1000 + resumes % 5000;
        }
        for (int i = 0; i < 16; i++) fold(ctx.r[i]);
        fold(ctx.n | ctx.z << 8 | ctx.c << 16 | (uint64_t)ctx.v << 24 | (uint64_t)ctx.q << 32 | (uint64_t)ctx.thumb << 40 | (uint64_t)ctx.ge << 48);
        fold(((uint64_t)(uint32_t)ctx.budget << 32) | ctx.depth);
        for (int i = 0; i < 32; i++) fold(vfp[i]);
        fold(fpscr);
        /* only pages written count, in order of address: a load the C
           compiler found dead and left out touches no page where the
           assembly's does, or touches it later */
        qsort(pages, page_count, sizeof pages[0], by_number);
        for (int p = 0; p < page_count; p++) {
            uint64_t offset = (uint64_t)pages[p] << 12;
            int written = 0;
            for (int i = 0; i < 512 && !written; i++) written = ((const uint64_t *)(view + offset))[i] != mix(offset + (uint64_t)i);
            if (!written) continue;
            if (t == traced) {
                uint64_t h = 0;
                const uint64_t *w = (const uint64_t *)(view + ((uint64_t)pages[p] << 12));
                for (int i = 0; i < 512; i++) h = mix(h ^ w[i]);
                printf("page %05x %016llx\n", pages[p], (unsigned long long)h);
            }
            fold(pages[p]);
            const uint64_t *words = (const uint64_t *)(view + ((uint64_t)pages[p] << 12));
            for (int i = 0; i < 512; i++) fold(words[i]);
        }
        printf("%d %d %016llx\n", t, resumes, (unsigned long long)trace);
        munmap(view, SPAN + 4096);
    }
    return 0;
}
