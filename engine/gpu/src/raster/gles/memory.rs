//! Optional cache attribution. Reachable CPU buffers can also be owned by
//! non-GLES caches; these counts are not additive with process RSS or VRAM.

use super::*;

pub(super) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("ZAKURO_GPU_MEMORY").is_some())
}

/// Counts a shared allocation once within this snapshot, including buffers
/// retained by more than one texture, surface, or table entry.
fn shared_bytes(
    seen: &mut std::collections::HashSet<usize>,
    pointer: usize,
    bytes: usize,
) -> usize {
    if bytes != 0 && seen.insert(pointer) {
        bytes
    } else {
        0
    }
}

fn pool_bytes<T>(pool: &Pool<T>) -> usize {
    pool.0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .map(|buffer| buffer.capacity() * std::mem::size_of::<T>())
        .sum()
}

impl Gles {
    pub(super) fn report_memory(&self) {
        let mut seen = std::collections::HashSet::new();
        let shadows: usize = self
            .surfaces
            .iter()
            .map(|surface| surface.shadow.capacity())
            .sum();
        let pictures: usize = self
            .surfaces
            .iter()
            .filter_map(|surface| surface.picture.as_ref())
            .map(|(_, bytes)| {
                shared_bytes(&mut seen, Arc::as_ptr(bytes) as usize, bytes.capacity())
            })
            .sum();
        let mut texture_bytes = 0;
        for texture in self.textures.values() {
            let (texels, packed) = &texture._texels;
            texture_bytes += shared_bytes(
                &mut seen,
                Arc::as_ptr(texels) as *const u8 as usize,
                texels.len() * 4,
            );
            if let Some((_, bytes)) = packed {
                texture_bytes += shared_bytes(
                    &mut seen,
                    Arc::as_ptr(bytes) as *const u8 as usize,
                    bytes.len(),
                );
            }
        }
        let mut table_bytes = 0;
        for (_, bytes) in self.table_sets.values() {
            table_bytes += shared_bytes(
                &mut seen,
                Arc::as_ptr(bytes) as *const u8 as usize,
                bytes.len(),
            );
        }
        if let Some((_, bytes)) = &self.tables_due {
            table_bytes += shared_bytes(
                &mut seen,
                Arc::as_ptr(bytes) as *const u8 as usize,
                bytes.len(),
            );
        }
        let over_drawing: usize = self.over_drawing.values().map(Vec::capacity).sum();
        let sources: usize = self.vertex_sources.values().map(String::capacity).sum();
        let inputs: usize = self.inputs.values().map(String::capacity).sum();
        let key_arrays: usize = self
            .programs
            .keys()
            .chain(self.inputs.keys())
            .map(|key| key.2.capacity() * 4)
            .sum();
        let pending = self
            .compiler
            .as_ref()
            .map_or(0, |compiler| compiler.pending.len());
        let recent = self.flushes.saturating_sub(PROGRAM_KEEP_FLUSHES);
        let never_drawn = self
            .programs
            .values()
            .filter(|program| !self.program_used.contains_key(program))
            .count();
        let unused = self
            .programs
            .values()
            .filter(|program| {
                self.program_used
                    .get(program)
                    .is_some_and(|&used| used < recent)
            })
            .count();
        let vertices = self
            .programs
            .keys()
            .map(|key| key.3)
            .collect::<std::collections::HashSet<_>>()
            .len();
        let stages = STAGE_PROGRAMS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map_or(0, |stages| stages.len());
        let pairs = SEPARATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map_or(0, |pairs| pairs.len());
        log::info!("gpu memory snapshot: batch {}, CPU reachable unique buffer bytes shadows {shadows}, pictures {pictures}, textures {texture_bytes}, tables {table_bytes}, over_drawing {over_drawing}, vertex_sources {sources}, inputs {inputs}, program_key_arrays {key_arrays}; GL objects surfaces {}, textures {}, programs {}, copies {}, pending {pending}; excludes driver allocations, cache bookkeeping and other owners", self.flushes, self.surfaces.len(), self.textures.len(), self.programs.len(), self.copies.len());
        log::info!("gpu memory separable snapshot: process-wide cached stage programs {stages}, stage pair tokens {pairs}; driver executable allocation bytes unknown");
        log::info!("gpu memory program lifetime snapshot: {} unique vertex source keys, {never_drawn} programs never drawn, {unused} programs unused for more than {PROGRAM_KEEP_FLUSHES} batches; proactive retirement keeps admission grace and generic stand-ins", vertices);
        log::info!("gpu memory reusable pool snapshot: word buffers {} bytes, byte buffers {} bytes, held draw buffers {} bytes", pool_bytes(&WORDS), pool_bytes(&BYTES), pool_bytes(&DRAWN));
        self.worker.post_keeping(|| {
            local(|local| {
                let ring_set = [&local.uniforms, &local.shading, &local.inputs, &local.indices, &local.vertices, &local.tables];
                let rings: usize = ring_set.iter().map(|ring| ring.size).sum();
                let mapped = ring_set.iter().filter(|ring| ring.mapped.is_some()).count();
                let reachable_arrays: usize = local.resident.buffers.values().map(|(_, bytes, _)| bytes.len()).sum();
                log::info!("gpu memory GL-thread snapshot: resident buffers {}, page-rounded bytes {}, reachable CPU array bytes {reachable_arrays}, ring buffer capacity bytes {rings}; persistent mapped rings {mapped}/6; driver overhead and orphaned/in-flight storage unknown", local.resident.buffers.len(), local.resident.bytes);
            });
        });
    }
}
