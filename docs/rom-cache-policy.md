# Opt-in ROM cache policy

`ZAKURO_ROM_CACHE_MIB` accepts ASCII decimal integers 0 through 1024. Absent or
zero preserves the ordinary mmap behavior. Invalid values warn and disable the
policy. Positive values enable a best-effort LRU of 1 MiB ROM chunks on 64-bit
Linux/Android with supported host-page geometry.

After a guest file read has copied its bytes into guest memory, all overlapping
chunks become recent, including reads smaller than a host page. When the chunk
budget is exceeded, full host pages inside the oldest tracked chunk are retired.
This includes adjacent kernel read-ahead and pages only partly covered by guest
reads. Chunks never tracked by completed reads are left alone. Opening the ROM
does not flush its contents.

Once `Title::code()` successfully creates its owned executable Vec, the exact
raw ExeFS `.code` source range is also retired. Only fully enclosed host pages
are eligible, so adjacent entries and partial boundary pages remain ordinary.
A decompression error does not retire the source. The Vec already contains an
independent output before this advice; later reads of `.code` reload its original
bytes from the ROM.

Retirement first requests mmap `MADV_DONTNEED`, then requests
`POSIX_FADV_DONTNEED` on the original file descriptor. Both requests are necessary:
dropping mapped pages alone can lower process RSS while leaving physical file
cache resident. Neither request modifies the ROM. Borrowed read-only slices
remain valid and can fault unchanged bytes back from the file. As with the
existing mmap loader, the ROM must remain unchanged while open.

This is a chunk working-set target, not a strict physical-memory cap. Kernel
read-ahead, shared mappings, other readers and ignored advice can exceed it.
Clean file-backed RSS is reclaimable by the kernel even with this policy off.
`POSIX_FADV_DONTNEED` concerns the shared file cache and can affect other readers.
Lower process RSS is not an equal reduction in anonymous memory, total system
RAM or GPU allocations. Small budgets can increase storage I/O and page faults;
SD-card and Cortex-A55 costs have not been measured. Compare full-game wall/CPU
time, fault/I/O observations and correctness alongside memory. The default stays
off; use a nonzero budget only when its memory/I/O trade-off suits the system.

Successful initialization logs:

```text
ROM cache enabled: budget_mib=4 chunk_mib=1 host_page_bytes=4096 eviction_scope=old_tracked_chunks code_copy_retirement=true best_effort=true
```

Shutdown counters record advice requests and errors, not bytes proven freed.
The explicit ignored disk-backed FS test checks physical cache with `mincore`,
unchanged byte rereads, retained recent chunks, untouched chunks and exact copy
boundaries. Its denied-file-advice negative control must fail even when mmap RSS
drops. Run the explicitly ignored disk-backed test on a qualified local filesystem;
it is not a portable CI assertion. Local receipts remain private. Reproduction
and measurement boundaries are described in [performance-eval.md](performance-eval.md).
