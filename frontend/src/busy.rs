//! how busy each thread and the GPU were between two log lines.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

pub struct Busy {
    ticks: HashMap<String, (String, u64)>,
    since: Instant,
    /// the GPU's load summed, how many reads, and its clock summed in MHz,
    /// and whether there is a load, the H700's giving only the clock.
    gpu: Option<(Arc<(AtomicU64, AtomicU64, AtomicU64)>, bool)>,
}

impl Busy {
    pub fn start() -> Busy {
        // the GPU's devfreq load, "NN@FREQHz", averaged from a read every 100 ms
        let load = std::fs::read_dir("/sys/class/devfreq").ok().and_then(|dirs| {
            dirs.flatten().map(|dir| dir.path()).find(|path| path.to_string_lossy().contains("gpu") && (path.join("load").exists() || path.join("cur_freq").exists()))
        });
        let gpu = load.map(|dir| {
            let loads = dir.join("load").exists();
            let samples = Arc::new((AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)));
            let taken = samples.clone();
            let _ = std::thread::Builder::new().name("gpu-load".into()).spawn(move || loop {
                // the load is of the clock the GPU runs at then, which the
                // part after the @ says: a GPU held at a low clock reads busy
                let sample = if loads {
                    std::fs::read_to_string(dir.join("load")).ok().and_then(|text| {
                        let (load, hz) = text.trim().split_once('@').unwrap_or((text.trim(), ""));
                        Some((load.trim().parse::<u64>().ok()?, hz.trim_end_matches("Hz").trim().parse::<u64>().unwrap_or(0)))
                    })
                } else {
                    std::fs::read_to_string(dir.join("cur_freq")).ok().and_then(|text| Some((0, text.trim().parse::<u64>().ok()?)))
                };
                if let Some((load, hz)) = sample {
                    taken.0.fetch_add(load, Ordering::Relaxed);
                    taken.1.fetch_add(1, Ordering::Relaxed);
                    taken.2.fetch_add(hz / 1_000_000, Ordering::Relaxed);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            });
            (samples, loads)
        });
        let mut busy = Busy { ticks: HashMap::new(), since: Instant::now(), gpu };
        busy.report();
        busy
    }

    /// the busiest threads' share of a core and the GPU's load since the
    /// last report.
    pub fn report(&mut self) -> String {
        let seconds = std::mem::replace(&mut self.since, Instant::now()).elapsed().as_secs_f64().max(0.001);
        let mut used = Vec::new();
        for task in std::fs::read_dir("/proc/self/task").into_iter().flatten().flatten() {
            let Ok(stat) = std::fs::read_to_string(task.path().join("stat")) else { continue };
            let Some((head, rest)) = stat.rsplit_once(')') else { continue };
            let name = head.split_once('(').map_or("", |(_, name)| name).to_owned();
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let ticks = fields.get(11).and_then(|t| t.parse::<u64>().ok()).unwrap_or(0) + fields.get(12).and_then(|t| t.parse::<u64>().ok()).unwrap_or(0);
            let tid = task.file_name().to_string_lossy().into_owned();
            let before = self.ticks.insert(tid, (name.clone(), ticks)).map_or(ticks, |(_, before)| before);
            used.push((ticks - before, name));
        }
        used.sort_by_key(|&(ticks, _)| std::cmp::Reverse(ticks));
        let mut out: Vec<String> = used.iter().take(4).map(|(ticks, name)| format!("{name} {:.0}%", *ticks as f64 / seconds)).collect();
        let waited = zakuro_core::GPU_WAITED.swap(0, Ordering::Relaxed) as f64 / 1e6;
        out.push(format!("waiting on gpu {:.0}%", waited / seconds * 100.0));
        if let Some((gpu, loads)) = &self.gpu {
            let (sum, count, mhz) = (gpu.0.swap(0, Ordering::Relaxed), gpu.1.swap(0, Ordering::Relaxed), gpu.2.swap(0, Ordering::Relaxed));
            if let (Some(load), Some(mhz)) = (sum.checked_div(count), mhz.checked_div(count)) {
                out.push(if !loads {
                    format!("GPU at {mhz} MHz")
                } else if mhz > 0 { format!("GPU {load}% at {mhz} MHz") } else { format!("GPU {load}%") });
            }
        }
        // what the game holds of the memory and what the system has left,
        // the driver's memory counted in the first on a handheld
        let resident = std::fs::read_to_string("/proc/self/statm").ok().and_then(|statm| statm.split_whitespace().nth(1)?.parse::<u64>().ok());
        let available = std::fs::read_to_string("/proc/meminfo").ok().and_then(|info| info.lines().find_map(|line| line.strip_prefix("MemAvailable:")?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()));
        if let (Some(pages), Some(kib)) = (resident, available) {
            // SAFETY: a plain query
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
            out.push(format!("memory {} MiB, {} MiB free", (pages * page) >> 20, kib >> 10));
        }
        out.join(", ")
    }
}
