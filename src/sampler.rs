//! Background sampling thread.
//!
//! One thread, one `System`, one timer. Everything the UI needs arrives as a
//! `Snapshot` over a bounded channel so a stalled window can never make the
//! queue grow without limit.
//!
//! Instruments are discovered at runtime rather than fixed at compile time:
//! every adapter, every volume and every GPU becomes its own channel, because
//! "network 12 MB/s" says nothing about which of three interfaces is busy.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{Disks, Gpus, Networks, Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::win_sensors::{volume_info, MemReading, Sensors, VolumeInfo};
use crate::util::{fmt_capacity, shorten};

pub const CH_CPU: &str = "cpu";
pub const CH_CORE: &str = "core";
pub const CH_MEM: &str = "mem";
/// Committed memory against the commit limit. Windows has no swap counter;
/// this is the figure that fills up first and the one Task Manager labels
/// "Committed".
pub const CH_COMMIT: &str = "commit";
pub const CH_SWAP: &str = "swap";

/// Which family a channel belongs to. The UI groups the watchlist by this and
/// picks the matching per-process column from it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Cpu,
    Core,
    Mem,
    Commit,
    Swap,
    Gpu,
    Net,
    Disk,
}

/// One tradable line. `value` is what the candles are made of; `flow` is what
/// the volume bars under them measure.
#[derive(Clone)]
pub struct Channel {
    /// Stable across refreshes; the UI keys history on it.
    pub id: String,
    pub label: String,
    pub desc: String,
    /// Extra identity shown next to the value in the watchlist row. Empty for
    /// instruments whose label already says what they are.
    pub note: String,
    pub kind: Kind,
    pub value: f32,
    /// Unit the value is quoted in: `"%"`, `"GB"`, or empty for a rate whose
    /// unit the subtitle already names. It drives formatting *and* whether the
    /// chart clamps its axis to 0..100 — memory is plotted in GB and must be
    /// allowed to use the range it actually occupies.
    pub unit: &'static str,
    /// Ceiling the value is a share of, when there is one. Lets the row read
    /// "9.84 / 15.7 GB" without a per-instrument special case in the UI.
    pub total: Option<f32>,
    pub dec: usize,
    pub flow: f32,
}

#[derive(Clone, Default)]
pub struct GpuStat {
    pub id: String,
    pub name: String,
    pub usage: f32,
    /// Memory in use, in MB. On a discrete card this is dedicated VRAM; on an
    /// integrated one it is the slice borrowed from system RAM.
    pub vram_used_mb: f32,
    /// Usable capacity in MB, or 0 when the adapter has no meaningful ceiling
    /// (an iGPU can borrow up to half of RAM, which is not a VRAM size).
    pub vram_total_mb: f32,
    pub temp_c: Option<f32>,
    pub power_w: Option<f32>,
}

/// Which per-process figure the position ladder ranks and displays. The UI
/// switches this when the instrument changes, so the ladder always shows a
/// number that exists for the selected metric instead of a column of CPU%.
pub const METRIC_CPU: u8 = 0;
pub const METRIC_MEM: u8 = 1;
pub const METRIC_GPU: u8 = 2;

#[derive(Clone, Default)]
pub struct ProcRow {
    pub pid: u32,
    pub name: String,
    /// Instantaneous share of total machine capacity.
    pub cpu: f32,
    /// Smoothed share. This is what the ladder is ordered by, so it is also
    /// what it displays: a list sorted by one number while showing another
    /// looks broken even when the ordering is right.
    pub avg: f32,
    /// Resident memory in MB. Only refreshed while the memory view is up.
    pub mem_mb: f32,
    /// GPU utilization share. Only refreshed while the GPU view is up.
    pub gpu_pct: f32,
}

/// Weight of the newest sample in the ranking average. The process table is
/// re-enumerated every 2 s, so 0.25 gives roughly an 8 s memory: long enough to
/// ride out a single-tick blip, short enough that a real hog shows up in time.
const RANK_ALPHA: f32 = 0.25;

/// Width of one ranking band, as a share of total machine capacity. Rows in the
/// same band are tied and keep their previous order, which is what stops the
/// ladder reshuffling on every refresh.
///
/// Fixed rather than relative to the peak on purpose: a step that shrinks with
/// the peak re-bands every row at once, and the whole list jumps even though
/// nothing changed.
const RANK_BAND_PCT: f32 = 0.4;

/// Floor for appearing on the ladder at all, applied to the smoothed value.
/// 0.02 % of the machine is 0.16 % of one core — small enough to keep light but
/// real load in the list, high enough to skip pure zeros.
const RANK_ENTRY_PCT: f32 = 0.02;

/// A single device cannot move more than this. Counters are 64-bit and real
/// hardware tops out far below it, so anything larger is a driver glitch and
/// plotting it would flatten every other candle on the chart.
const MAX_RATE_MBPS: f32 = 10_000.0;

/// Rolling state the ranking needs across refreshes. Without it every tick is
/// ranked from scratch, which is the equivalent of sorting a market by its last
/// trade instead of by the day's change.
#[derive(Default)]
struct Ranker {
    ema: HashMap<u32, f32>,
    prev: HashMap<u32, usize>,
}

/// Per-device byte counter differencing.
///
/// Differencing a *sum* over all devices is what produced the absurd readings:
/// plug in a USB stick that has already moved 100 GB in its life, and the sum
/// jumps by 100 GB in one tick — tens of GB/s, and a four-digit percentage
/// change. Keying by device and seeding the baseline on first sight means a new
/// or reconnected device contributes nothing until it actually moves bytes.
#[derive(Default)]
struct RateMeter {
    last: HashMap<String, (u64, Instant)>,
}

impl RateMeter {
    fn sample(&mut self, key: &str, counter: u64, now: Instant) -> f32 {
        match self.last.get_mut(key) {
            Some((prev, at)) => {
                let dt = (now - *at).as_secs_f32().max(0.001);
                let prev = std::mem::replace(prev, counter);
                *at = now;
                // A counter that went backwards was reset: the device was
                // re-plugged, the adapter restarted, or it wrapped. Reseeding
                // above is the whole handling — this tick reads zero.
                if counter < prev {
                    return 0.0;
                }
                ((counter - prev) as f32 / 1_048_576.0 / dt).min(MAX_RATE_MBPS)
            }
            None => {
                self.last.insert(key.to_string(), (counter, now));
                0.0
            }
        }
    }

    /// Forgets devices that are no longer present, so an unplugged stick does
    /// not leave an entry behind for the rest of the session.
    fn retain(&mut self, live: &HashSet<String>) {
        self.last.retain(|k, _| live.contains(k));
    }
}

/// Intent sent from the UI to the sampler. Terminating a process needs the live
/// `System`, which only the sampler thread owns, so the window never touches it.
pub enum Order {
    Close(u32),
}

/// Result of an order, echoed back to the UI as a fill report.
#[derive(Clone)]
pub struct Fill {
    pub pid: u32,
    pub name: String,
    pub cpu: f32,
    pub ok: bool,
    pub error: Option<&'static str>,
}

#[derive(Clone)]
pub struct Snapshot {
    /// Unix seconds, used as the candle bucket key.
    pub wall: i64,
    /// Every instrument, in watchlist order.
    pub channels: Vec<Channel>,
    /// Adapters, for the per-GPU sensor strip.
    pub gpus: Vec<GpuStat>,
    pub cores: Vec<f32>,
    pub procs: Vec<ProcRow>,
    /// Bumped only when the process table is actually re-enumerated. The UI
    /// needs this to avoid pushing the same sample eight times per refresh.
    pub procs_gen: u64,
    pub self_cpu: f32,
    pub self_mem: f32,
    pub cpu_brand: String,
    pub ncores: usize,
    pub total_mem_gb: f32,
    pub used_mem_gb: f32,
    pub host: String,
    pub os: String,
    pub disk_mbps: f32,
}

pub struct Handle {
    pub rx: Receiver<Snapshot>,
    pub fills: Receiver<Fill>,
    orders: SyncSender<Order>,
    metric: Arc<AtomicU8>,
}

impl Handle {
    /// Fires and forgets; the outcome comes back on `fills`.
    pub fn close(&self, pid: u32) {
        let _ = self.orders.try_send(Order::Close(pid));
    }

    /// Switches what the ladder measures. Reading memory for the whole process
    /// table costs more than reading CPU alone, so it only happens while the
    /// memory view is open.
    pub fn set_metric(&self, metric: u8) {
        self.metric.store(metric, AtomicOrdering::Relaxed);
    }
}

/// Starts the sampler. `interval_ms` is the raw sample period; 250 ms gives
/// four samples per 1s candle, which is enough for a meaningful wick.
pub fn spawn(interval_ms: u64) -> Handle {
    let (tx, rx) = sync_channel::<Snapshot>(4);
    let (fill_tx, fills) = sync_channel::<Fill>(16);
    let (orders, order_rx) = sync_channel::<Order>(8);
    let tick = Duration::from_millis(interval_ms.clamp(50, 5_000));
    let metric = Arc::new(AtomicU8::new(METRIC_CPU));
    let thread_metric = Arc::clone(&metric);

    thread::Builder::new()
        .name("fxviewer-sampler".to_string())
        .spawn(move || run(tx, fill_tx, order_rx, tick, thread_metric))
        .expect("failed to spawn sampler thread");

    Handle {
        rx,
        fills,
        orders,
        metric,
    }
}

/// One volume as it appears in the watchlist.
struct DiskRow {
    key: String,
    label: String,
    note: String,
    desc: String,
    rate: f32,
}

/// One adapter as it appears in the watchlist.
struct NetRow {
    key: String,
    label: String,
    desc: String,
    rate: f32,
}

fn run(
    tx: SyncSender<Snapshot>,
    fill_tx: SyncSender<Fill>,
    orders: Receiver<Order>,
    tick: Duration,
    metric: Arc<AtomicU8>,
) {
    let mut sys = match System::new() {
        Ok(s) => s,
        // Nothing sensible to do without it, and the window is better off
        // showing zeros than the process dying.
        Err(_) => return,
    };
    sys.refresh_cpu_usage();
    sys.refresh_memory();

    // CPU deltas need a full period between refreshes, otherwise the very
    // first reading is computed over a near-zero interval and comes out
    // meaningless.
    thread::sleep(tick);

    let mut nets = match Networks::new_with_refreshed_list() {
        Ok(n) => n,
        Err(_) => return,
    };
    let mut net_meter = RateMeter::default();
    let mut net_rows: Vec<NetRow> = Vec::new();

    let mut disks = match Disks::new_with_refreshed_list() {
        Ok(d) => d,
        Err(_) => return,
    };
    let mut disk_meter = RateMeter::default();
    let mut disk_rows: Vec<DiskRow> = Vec::new();
    let mut volume_cache: HashMap<String, VolumeInfo> = HashMap::new();

    // GPU is optional: a machine with no adapter, or a driver that hides the
    // counters, simply reports nothing.
    let mut gpus = Gpus::new_with_refreshed_list().ok();
    let mut gpu_rows: Vec<GpuStat> = Vec::new();
    let mut sensors = Sensors::new();
    // Commit charge moves at memory-allocation speed, not at sampling speed:
    // once a second is plenty, and the PDH collection is the most expensive
    // thing in this loop after the process table.
    let mut mem_reading: Option<MemReading> = None;

    let pid = Pid::from_u32(std::process::id());
    let brand = sys
        .cpus()
        .first()
        .map(|c| c.brand().trim().to_string())
        .unwrap_or_else(|| "unknown cpu".to_string());

    let host = System::host_name().unwrap_or_else(|_| "unknown".to_string());
    let os = match (System::name(), System::os_version()) {
        (Ok(n), Ok(v)) => format!("{} {}", n, v),
        (Ok(n), Err(_)) => n,
        _ => "unknown".to_string(),
    };

    let mut procs: Vec<ProcRow> = Vec::new();
    let mut procs_gen: u64 = 0;
    let mut ranker = Ranker::default();
    let mut tick_n: u32 = 0;
    let mut next = Instant::now();

    loop {
        next += tick;

        // Orders are drained before sampling so a close is reflected within the
        // same tick rather than a full period later.
        while let Ok(Order::Close(target)) = orders.try_recv() {
            let target_pid = Pid::from_u32(target);
            // The full table only refreshes every 8th tick, so a pid that is
            // alive can be absent from the cached snapshot. One targeted
            // refresh — kill is a rare path, one pid costs nothing — makes the
            // verdict independent of the refresh cadence.
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[target_pid]),
                true,
                ProcessRefreshKind::nothing(),
            );
            let known = procs.iter().find(|r| r.pid == target);
            let mut name = known.map(|r| r.name.clone()).unwrap_or_default();
            let cpu = known.map(|r| r.cpu).unwrap_or(0.0);

            let (ok, error) = match sys.process(target_pid) {
                Some(p) => {
                    if name.is_empty() {
                        name = p.name().to_string_lossy().into_owned();
                    }
                    if p.kill() {
                        (true, None)
                    } else {
                        // Almost always a privilege problem: Task Manager would
                        // need "run as administrator" to do the same thing.
                        (false, Some("access denied"))
                    }
                }
                None => (false, Some("position already closed")),
            };

            let _ = fill_tx.send(Fill {
                pid: target,
                name,
                cpu,
                ok,
                error,
            });
        }

        sys.refresh_cpu_usage();
        sys.refresh_memory();
        nets.refresh(true);

        let now = Instant::now();
        tick_n = tick_n.wrapping_add(1);
        let view = metric.load(AtomicOrdering::Relaxed);

        // ---- per-adapter throughput -------------------------------------
        net_rows.clear();
        let mut live = HashSet::new();
        for (name, data) in nets.iter() {
            let full = name.to_string_lossy().into_owned();
            let total = data
                .total_received()
                .saturating_add(data.total_transmitted());
            let rate = net_meter.sample(&full, total, now);
            live.insert(full.clone());
            net_rows.push(NetRow {
                key: full.clone(),
                label: nic_label(&full),
                desc: "rx + tx".to_string(),
                rate,
            });
        }
        net_meter.retain(&live);
        net_rows.sort_by(|a, b| a.key.cmp(&b.key));

        // Disk is a volume walk. Sampling it every tick gives a 1 s candle four
        // samples instead of one, which is the difference between a candlestick
        // and a row of dots. Measured: 0.29 ms per refresh, 0.1 % of one core
        // at 4 Hz — cheap enough to stop trading detail for it.
        const DISK_EVERY: u32 = 1;
        if tick_n % DISK_EVERY == 0 {
            disks.refresh(true);
            disk_rows.clear();
            let mut live = HashSet::new();
            for d in disks.iter() {
                if d.total_space() == 0 {
                    continue;
                }
                let mount = d.mount_point().to_string_lossy().into_owned();
                let letter = disk_label(&mount);
                let key = mount.clone();
                // Two read-only ioctls, once per volume: the physical disk
                // number and the device's product string. Cached because the
                // answer only changes when the hardware does.
                let info = volume_cache
                    .entry(key.clone())
                    .or_insert_with(|| volume_info(&mount).unwrap_or_default());
                let u = d.usage();
                let total = u.total_read_bytes.saturating_add(u.total_written_bytes);
                let rate = disk_meter.sample(&key, total, now);
                live.insert(key.clone());

                let label = match info.disk_no {
                    Some(n) => format!("Disk {} ({})", n, letter),
                    None => letter.clone(),
                };
                let bus = info.bus.unwrap_or("");
                let media = match d.kind() {
                    sysinfo::DiskKind::SSD => "SSD",
                    sysinfo::DiskKind::HDD => "HDD",
                    _ => "",
                };
                // A short product string is the best identity a drive can
                // offer ("SDHC Card"); otherwise the bus plus the media type
                // ("NVMe SSD") says more than a model number cut in half.
                let product = info.product.clone();
                let fits = product
                    .as_deref()
                    .map(|p| p.chars().count() <= 14)
                    .unwrap_or(false);
                let note = if fits {
                    product.clone().unwrap_or_default()
                } else if !bus.is_empty() && !media.is_empty() {
                    format!("{} {}", bus, media)
                } else if !bus.is_empty() {
                    bus.to_string()
                } else if !media.is_empty() {
                    media.to_string()
                } else {
                    product
                        .as_deref()
                        .map(|p| shorten(p, 14))
                        .unwrap_or_default()
                };

                let mut desc = String::from("read + write");
                let label_text = d.name().to_string_lossy().into_owned();
                if !label_text.is_empty() {
                    desc.push_str(" · ");
                    desc.push_str(&shorten(&label_text, 18));
                }
                match product.as_deref() {
                    Some(p) => {
                        desc.push_str(" · ");
                        desc.push_str(&shorten(p, 26));
                    }
                    None if !bus.is_empty() => {
                        desc.push_str(" · ");
                        desc.push_str(bus);
                    }
                    _ => {}
                }
                desc.push_str(&format!(" · {}", fmt_capacity(d.total_space())));
                if d.is_removable() {
                    desc.push_str(" · removable");
                }
                disk_rows.push(DiskRow {
                    key,
                    label,
                    note,
                    desc,
                    rate,
                });
            }
            disk_meter.retain(&live);
            volume_cache.retain(|k, _| live.contains(k));
            disk_rows.sort_by(|a, b| a.key.cmp(&b.key));
        }
        let disk_mbps = disk_rows.iter().map(|d| d.rate).sum::<f32>();

        // GPU is not cheap. One refresh costs roughly 100 ms of CPU — the DXGI
        // enumeration and PDH query behind it are far heavier than everything
        // else this program does combined. So it runs once a second while the
        // GPU instrument is on screen, and once every 15 s otherwise, which is
        // enough to keep the watchlist row roughly current.
        //
        // One second is also as fast as the source goes: sysinfo reads the
        // `\GPU Engine(*)\Utilization Percentage` PDH counters, and Windows
        // refreshes those once per second. At the 1 s timeframe that means one
        // distinct reading per candle — a legitimate doji, which is why flat
        // GPU candles are the data being honest, not a rendering bug.
        let gpu_every = if view == METRIC_GPU { 4 } else { 60 };
        if tick_n % gpu_every == 1 {
            if let Some(g) = &mut gpus {
                g.refresh(false);
                // Shared memory is a machine-wide counter set: only meaningful
                // to attribute when there is a single adapter to attribute it
                // to.
                let shared = if g.len() == 1 {
                    sensors.shared_memory() as f32 / 1_048_576.0
                } else {
                    0.0
                };
                // Adapter order is stable, so the previous row is the right
                // fallback when the driver has no reading this time.
                let previous = std::mem::take(&mut gpu_rows);
                for (i, dev) in g.iter().enumerate() {
                    // The first refresh has no interval to diff against and
                    // reports `None`. Publishing that as 0 % draws a fake dip
                    // into the chart and a 0 % row into the watchlist, so the
                    // adapter is skipped until it has a real reading.
                    let Some(usage) = dev.usage().or_else(|| previous.get(i).map(|r| r.usage))
                    else {
                        continue;
                    };
                    let id = dev.pci().to_string();
                    let name = clean_name(
                        dev.model().or_else(|| dev.vendor()).unwrap_or("gpu"),
                    );
                    let mut row = GpuStat {
                        id,
                        name: name.clone(),
                        usage,
                        vram_used_mb: dev.used_memory().unwrap_or(0) as f32 / 1_048_576.0,
                        vram_total_mb: dev.total_memory().unwrap_or(0) as f32 / 1_048_576.0,
                        temp_c: None,
                        power_w: None,
                    };
                    // Vendor sensors win when present: NVML reports real
                    // capacity, and is the only source of temperature/power.
                    if let Some(r) = sensors.read_gpu(&name) {
                        if r.total > 0 {
                            row.vram_used_mb = r.used as f32 / 1_048_576.0;
                            row.vram_total_mb = r.total as f32 / 1_048_576.0;
                        }
                        row.temp_c = r.temp_c;
                        row.power_w = r.power_w;
                    } else if row.vram_total_mb < 1024.0 {
                        // Integrated adapter: the "total" sysinfo reports is
                        // the dedicated carve-out, not a usable ceiling, so it
                        // is dropped rather than shown as a 0.1 GB limit.
                        row.vram_used_mb += shared;
                        row.vram_total_mb = 0.0;
                    }
                    gpu_rows.push(row);
                }
            }
        }

        // Enumerating processes is by far the most expensive call, so it runs
        // at a slower cadence than the scalar counters.
        if procs.is_empty() || tick_n % 8 == 1 {
            // CPU alone for the whole table: asking for memory, disk and the
            // command line as well multiplies the cost of this call. The other
            // two columns are added back only while the ladder is showing them.
            let mut kind = ProcessRefreshKind::nothing().with_cpu();
            if view == METRIC_MEM {
                kind = kind.with_memory();
            }
            if view == METRIC_GPU {
                kind = kind.with_gpu_usage();
            }
            sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
            // Our own footprint is always on screen, so it is worth a second
            // targeted refresh rather than a memory read for every process.
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                false,
                ProcessRefreshKind::nothing().with_memory(),
            );
            let ncores = sys.cpus().len().max(1);
            procs = top_processes(&sys, ncores, &mut ranker, view);
            procs_gen += 1;
        }

        let cpus = sys.cpus();
        let n = cpus.len().max(1);
        let mut sum = 0.0f32;
        let mut peak = 0.0f32;
        let mut cores = Vec::with_capacity(n.min(64));
        for (i, c) in cpus.iter().enumerate() {
            let u = c.usage();
            sum += u;
            if u > peak {
                peak = u;
            }
            if i < 64 {
                cores.push(u);
            }
        }
        let cpu = sum / n as f32;

        let total_mem = sys.total_memory() as f32;
        let used_mem = sys.used_memory() as f32;
        let mem = if total_mem > 0.0 {
            used_mem / total_mem * 100.0
        } else {
            0.0
        };

        let total_swap = sys.total_swap() as f32;
        let used_swap = sys.used_swap() as f32;

        let (self_cpu, self_mem) = match sys.process(pid) {
            Some(p) => (
                p.cpu_usage() / n as f32,
                p.memory() as f32 / 1_048_576.0,
            ),
            None => (0.0, 0.0),
        };

        let total_mem_gb = total_mem / 1_073_741_824.0;
        let used_mem_gb = used_mem / 1_073_741_824.0;
        let total_swap_gb = total_swap / 1_073_741_824.0;
        let used_swap_gb = used_swap / 1_073_741_824.0;
        // The commit probe costs more than every other reading in the loop, so
        // it runs at full rate only while a memory instrument is on screen and
        // at half a hertz otherwise. A row that lags two seconds is still
        // telling the truth about a number that moves this slowly.
        let mem_due = view == METRIC_MEM || tick_n % 8 == 1;
        if mem_due {
            mem_reading = sensors.memory(total_swap as u64);
        }
        let commit = mem_reading;

        let mut channels: Vec<Channel> =
            Vec::with_capacity(4 + gpu_rows.len() + net_rows.len() + disk_rows.len());
        channels.push(Channel {
            id: CH_CPU.to_string(),
            label: "CPU".to_string(),
            desc: "total utilization".to_string(),
            note: String::new(),
            kind: Kind::Cpu,
            value: cpu,
            unit: "%",
            total: None,
            dec: 1,
            flow: disk_mbps,
        });
        channels.push(Channel {
            id: CH_CORE.to_string(),
            label: "CORE".to_string(),
            desc: "busiest core".to_string(),
            note: String::new(),
            kind: Kind::Core,
            value: peak,
            unit: "%",
            total: None,
            dec: 1,
            flow: disk_mbps,
        });
        // Memory and swap are plotted in GB, not percent: on a 16 GB machine
        // the interesting part of the curve lives between 8 and 12 GB, and a
        // 0..100 % axis squashes all of it into the middle of the pane. The
        // percentage is still the first thing in the row note.
        channels.push(Channel {
            id: CH_MEM.to_string(),
            label: "MEM".to_string(),
            desc: if total_mem_gb > 0.0 {
                format!("physical memory · {:.1} GB total", total_mem_gb)
            } else {
                "physical memory".to_string()
            },
            note: format!("{:.0}%", mem),
            kind: Kind::Mem,
            value: used_mem_gb,
            unit: "GB",
            total: (total_mem_gb > 0.0).then_some(total_mem_gb),
            dec: 2,
            flow: disk_mbps,
        });
        if let Some(c) = commit {
            let limit_gb = c.commit_limit as f32 / 1_073_741_824.0;
            let committed_gb = c.committed as f32 / 1_073_741_824.0;
            channels.push(Channel {
                id: CH_COMMIT.to_string(),
                label: "COMMIT".to_string(),
                desc: if limit_gb > 0.0 {
                    format!("committed · {:.1} GB limit", limit_gb)
                } else {
                    "committed memory".to_string()
                },
                note: if limit_gb > 0.0 {
                    format!("{:.0}%", committed_gb / limit_gb * 100.0)
                } else {
                    String::new()
                },
                kind: Kind::Commit,
                value: committed_gb,
                unit: "GB",
                total: (limit_gb > 0.0).then_some(limit_gb),
                dec: 2,
                flow: disk_mbps,
            });
        }
        // With PDH available this is what is actually inside pagefile.sys;
        // `sysinfo`'s swap figure is commit-over-RAM, which stays zero until
        // the machine is genuinely under pressure and therefore reads as a
        // dead instrument on a healthy box.
        let swap_gb = commit
            .map(|c| c.pagefile_used as f32 / 1_073_741_824.0)
            .unwrap_or(used_swap_gb);
        channels.push(Channel {
            id: CH_SWAP.to_string(),
            label: "SWAP".to_string(),
            desc: if total_swap_gb > 0.0 {
                format!("page file · {:.1} GB total", total_swap_gb)
            } else {
                "no page file".to_string()
            },
            note: if total_swap_gb > 0.0 {
                format!("{:.0}%", swap_gb / total_swap_gb * 100.0)
            } else {
                String::new()
            },
            kind: Kind::Swap,
            value: swap_gb,
            unit: if total_swap_gb > 0.0 { "GB" } else { "" },
            total: (total_swap_gb > 0.0).then_some(total_swap_gb),
            dec: 2,
            flow: disk_mbps,
        });
        for (i, g) in gpu_rows.iter().enumerate() {
            channels.push(Channel {
                id: format!("gpu:{}", g.id),
                label: if gpu_rows.len() == 1 {
                    "GPU".to_string()
                } else {
                    format!("GPU{}", i)
                },
                desc: shorten(&g.name, 32),
                note: shorten(&g.name, 18),
                kind: Kind::Gpu,
                value: g.usage,
                unit: "%",
                total: None,
                dec: 1,
                flow: disk_mbps,
            });
        }
        for nrow in &net_rows {
            channels.push(Channel {
                id: format!("net:{}", nrow.key),
                label: nrow.label.clone(),
                desc: nrow.desc.clone(),
                note: String::new(),
                kind: Kind::Net,
                value: nrow.rate,
                unit: "",
                total: None,
                dec: 2,
                flow: nrow.rate,
            });
        }
        for drow in &disk_rows {
            channels.push(Channel {
                id: format!("disk:{}", drow.key),
                label: drow.label.clone(),
                desc: drow.desc.clone(),
                note: drow.note.clone(),
                kind: Kind::Disk,
                value: drow.rate,
                unit: "",
                total: None,
                dec: 2,
                flow: drow.rate,
            });
        }

        let snap = Snapshot {
            wall: chrono::Local::now().timestamp(),
            channels,
            gpus: gpu_rows.clone(),
            cores,
            procs: procs.clone(),
            procs_gen,
            self_cpu,
            self_mem,
            cpu_brand: brand.clone(),
            ncores: n,
            total_mem_gb,
            used_mem_gb,
            host: host.clone(),
            os: os.clone(),
            disk_mbps,
        };

        if tx.send(snap).is_err() {
            break; // window closed
        }

        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}

/// Strips the trademark noise vendors put in adapter names, so the watchlist
/// row can carry the model without half of it being "(R)".
fn clean_name(s: &str) -> String {
    let t = s
        .replace("(R)", "")
        .replace("(TM)", "")
        .replace("(C)", "");
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    // "Intel Iris Xe Graphics" -> "Intel Iris Xe": the trailing noun carries no
    // information and costs the width of the watchlist note.
    match t.strip_suffix(" Graphics") {
        Some(short) => short.to_string(),
        None => t,
    }
}

/// "ZeroTier One [743993800f715420]" -> "ZeroTier One". Windows adapter names
/// carry a GUID suffix for virtual adapters, which is noise in a 198 px panel.
fn nic_label(full: &str) -> String {
    let base = match full.find(" [") {
        Some(i) => &full[..i],
        None => full,
    };
    let base = base.trim();
    if base.is_empty() {
        "nic".to_string()
    } else {
        shorten(base, 12)
    }
}

/// "C:\\" -> "C:", "/" stays "/".
fn disk_label(mount: &str) -> String {
    let t = mount.trim_end_matches(['\\', '/']);
    if t.is_empty() {
        mount.to_string()
    } else {
        t.to_string()
    }
}

fn top_processes(sys: &System, ncores: usize, rank: &mut Ranker, view: u8) -> Vec<ProcRow> {
    let n = ncores.max(1) as f32;
    let mut rows: Vec<ProcRow> = sys
        .processes()
        .iter()
        .map(|(pid, p)| ProcRow {
            pid: pid.as_u32(),
            name: p.name().to_string_lossy().into_owned(),
            cpu: p.cpu_usage() / n,
            avg: 0.0,
            mem_mb: p.memory() as f32 / 1_048_576.0,
            gpu_pct: p.gpu_usage().unwrap_or(0.0),
        })
        .collect();

    // Memory and GPU are stocks rather than flows: a process holding 2 GB or
    // 40% of the GPU is a fact, not a sample that might change next tick.
    // Ranking them directly needs none of the smoothing the CPU ladder does.
    if view == METRIC_MEM || view == METRIC_GPU {
        if view == METRIC_MEM {
            rows.retain(|r| r.mem_mb >= 1.0);
        } else {
            rows.retain(|r| r.gpu_pct > 0.05);
        }
        rows.sort_by(|a, b| {
            let (x, y) = if view == METRIC_MEM {
                (a.mem_mb, b.mem_mb)
            } else {
                (a.gpu_pct, b.gpu_pct)
            };
            y.partial_cmp(&x)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.name.cmp(&b.name))
        });
        rows.truncate(14);
        rank.prev.clear();
        for (i, r) in rows.iter().enumerate() {
            rank.prev.insert(r.pid, i);
        }
        return rows;
    }

    // Smooth before ranking. The displayed `cpu` stays instantaneous; only the
    // ordering uses the average.
    let live: HashSet<u32> = rows.iter().map(|r| r.pid).collect();
    rank.ema.retain(|pid, _| live.contains(pid));
    for r in &mut rows {
        let e = rank.ema.entry(r.pid).or_insert(r.cpu);
        *e = *e * (1.0 - RANK_ALPHA) + r.cpu * RANK_ALPHA;
        r.avg = *e;
    }

    // Membership is decided on the smoothed value, not the instantaneous one:
    // gating on the raw reading made rows on the noise floor flicker in and out
    // of the list every single refresh.
    rows.retain(|r| r.avg >= RANK_ENTRY_PCT);

    order_rows(&mut rows, &rank.prev, RANK_BAND_PCT);

    // Enough to fill the panel; the UI shows as many as fit the window height.
    rows.truncate(14);

    rank.prev.clear();
    for (i, r) in rows.iter().enumerate() {
        rank.prev.insert(r.pid, i);
    }
    rows
}

/// Sorts by smoothed load, but treats rows inside the same band as equal and
/// leaves them in their previous order.
///
/// This is the whole fix for the shuffling ladder: on an idle machine every row
/// sits in band zero, so nothing moves at all, while a process that genuinely
/// climbs still jumps bands and overtakes.
fn order_rows(rows: &mut [ProcRow], prev: &HashMap<u32, usize>, step: f32) {
    let step = if step > 0.0 { step } else { 1.0 };
    let band = |r: &ProcRow| (r.avg / step).floor();
    let seen = |pid: u32| prev.get(&pid).copied().unwrap_or(usize::MAX);

    rows.sort_by(|a, b| {
        band(b)
            .partial_cmp(&band(a))
            .unwrap_or(Ordering::Equal)
            // Same band: whoever was higher last time stays higher. A process
            // with no history sorts behind the ones already on the ladder.
            .then_with(|| seen(a.pid).cmp(&seen(b.pid)))
            .then_with(|| b.avg.partial_cmp(&a.avg).unwrap_or(Ordering::Equal))
            .then_with(|| a.name.cmp(&b.name))
    });
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// Exercises the whole order path against a real child process: the UI can
    /// only express intent, so a wiring mistake here would show up as a ticket
    /// that silently does nothing.
    #[test]
    fn close_order_terminates_the_target() {
        let mut child = Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn dummy process");
        let pid = child.id();

        let handle = spawn(100);
        std::thread::sleep(Duration::from_millis(600));
        handle.close(pid);

        let fill = handle
            .fills
            .recv_timeout(Duration::from_secs(5))
            .expect("no fill report came back");
        assert!(fill.ok, "close rejected: {:?}", fill.error);
        assert_eq!(fill.pid, pid);

        let status = child.wait().expect("child did not exit");
        assert!(!status.success() || status.code().is_some());
    }
}

#[cfg(test)]
mod rate_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_device_seen_for_the_first_time_contributes_nothing() {
        let mut m = RateMeter::default();
        let t0 = Instant::now();
        // 100 GB of lifetime traffic already on the counter.
        assert_eq!(m.sample("E:", 100_000_000_000, t0), 0.0);
        // One second later, 10 MiB moved: 10 MB/s, not 100 GB/s.
        let r = m.sample("E:", 100_000_000_000 + 10 * 1_048_576, t0 + Duration::from_secs(1));
        assert!((r - 10.0).abs() < 0.01, "got {r}");
    }

    #[test]
    fn a_reset_counter_reads_zero_instead_of_negative() {
        let mut m = RateMeter::default();
        let t0 = Instant::now();
        m.sample("WLAN", 5_000, t0);
        let r = m.sample("WLAN", 0, t0 + Duration::from_secs(1));
        assert_eq!(r, 0.0);
    }

    #[test]
    fn one_device_spike_does_not_affect_another() {
        let mut m = RateMeter::default();
        let t0 = Instant::now();
        m.sample("C:", 0, t0);
        m.sample("E:", 0, t0);
        m.sample("C:", 1_000_000, t0 + Duration::from_secs(1));
        let e = m.sample("E:", 0, t0 + Duration::from_secs(1));
        assert_eq!(e, 0.0);
    }

    #[test]
    fn labels_are_readable_on_a_198_px_panel() {
        assert_eq!(nic_label("WLAN"), "WLAN");
        assert_eq!(nic_label("ZeroTier One [743993800f715420]"), "ZeroTier One");
        assert_eq!(nic_label("以太网"), "以太网");
        assert_eq!(disk_label("C:\\"), "C:");
        assert_eq!(disk_label("/"), "/");
    }
}

#[cfg(test)]
mod rank_tests {
    use super::*;

    fn row(pid: u32, name: &str, avg: f32) -> ProcRow {
        ProcRow {
            pid,
            name: name.to_string(),
            cpu: avg,
            avg,
            mem_mb: 0.0,
            gpu_pct: 0.0,
        }
    }

    fn prev_of(pids: &[u32]) -> HashMap<u32, usize> {
        pids.iter().enumerate().map(|(i, p)| (*p, i)).collect()
    }

    fn order(rows: &[ProcRow]) -> Vec<u32> {
        rows.iter().map(|r| r.pid).collect()
    }

    #[test]
    fn near_ties_keep_their_previous_order() {
        // Beta is a shade busier right now, but 0.02% of a machine is noise.
        let mut rows = vec![row(1, "alpha", 0.10), row(2, "beta", 0.12)];
        order_rows(&mut rows, &prev_of(&[1, 2]), 0.4);
        assert_eq!(order(&rows), vec![1, 2]);
    }

    #[test]
    fn a_real_jump_overtakes() {
        let mut rows = vec![row(1, "alpha", 1.0), row(2, "beta", 40.0)];
        order_rows(&mut rows, &prev_of(&[1, 2]), 2.4);
        assert_eq!(order(&rows), vec![2, 1]);
    }

    #[test]
    fn idle_machine_holds_still() {
        // Every row is under the tie floor, so they share band zero and keep
        // whatever order they had, no matter how the values wobble.
        let mut rows = vec![
            row(1, "a", 0.30),
            row(2, "b", 0.05),
            row(3, "c", 0.19),
            row(4, "d", 0.11),
        ];
        order_rows(&mut rows, &prev_of(&[1, 2, 3, 4]), RANK_BAND_PCT);
        assert_eq!(order(&rows), vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_newcomer_sorts_behind_incumbents_in_its_band() {
        let mut rows = vec![row(9, "newcomer", 0.5), row(1, "old", 0.4)];
        order_rows(&mut rows, &prev_of(&[1]), 0.4);
        assert_eq!(order(&rows), vec![1, 9]);
    }

    #[test]
    fn displayed_value_never_contradicts_its_band() {
        // The number a row shows is the number it was ranked by, so two rows in
        // different bands can never appear in the wrong order.
        let mut rows = vec![
            row(1, "low", 0.9),
            row(2, "high", 12.0),
            row(3, "mid", 4.0),
        ];
        order_rows(&mut rows, &prev_of(&[1, 2, 3]), 1.0);
        let shown: Vec<f32> = rows.iter().map(|r| r.avg).collect();
        assert_eq!(shown, vec![12.0, 4.0, 0.9]);
    }
}
