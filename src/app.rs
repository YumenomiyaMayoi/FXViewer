//! Window layout: header, watchlist, process ladder, chart, status bar.
//!
//! Everything is drawn with the painter instead of stock widgets, which keeps
//! the visual language close to a real trading terminal and avoids a stack of
//! style overrides.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use eframe::egui::{self, pos2, vec2, Align2, FontId, Rect, Sense};

use crate::chart::{ChartOpts, ChartView};
use crate::sampler::{
    self, Channel, GpuStat, Kind, Snapshot, CH_COMMIT, CH_CORE, CH_CPU, CH_SWAP, METRIC_CPU,
    METRIC_GPU, METRIC_MEM,
};
use crate::series::{Candle, Series, TF_LABELS, TIMEFRAMES};
use crate::theme as th;
use crate::util::{self, fmt_change, fmt_delta_rate, fmt_num, fmt_pct, fmt_rate, shorten};

/// The per-process figure that matches an instrument. CPU, memory and GPU have
/// per-process readings; swap and the throughput instruments are machine-wide,
/// so those views keep the CPU column rather than showing a column of zeros.
fn metric_for(kind: Kind) -> u8 {
    match kind {
        Kind::Mem | Kind::Commit => METRIC_MEM,
        Kind::Gpu => METRIC_GPU,
        _ => METRIC_CPU,
    }
}

/// How a channel's value reads in the watchlist: the unit comes from the
/// channel, so a gigabyte instrument never prints "MB/s" and a percentage
/// instrument never prints a bare number.
fn fmt_value(unit: &str, total: Option<f32>, dec: usize, v: f32) -> String {
    match (unit, total) {
        ("%", _) => fmt_pct(v, dec),
        ("GB", Some(t)) => format!("{} / {:.1} GB", fmt_num(v, dec), t),
        ("GB", None) => format!("{} GB", fmt_num(v, dec)),
        _ => fmt_rate(v),
    }
}

/// Section heading for the watchlist. Instruments are grouped the way a
/// terminal groups asset classes, and the grouping is what makes ten rows
/// readable at a glance.
fn group_of(kind: Kind) -> &'static str {
    match kind {
        Kind::Cpu | Kind::Core => "COMPUTE",
        Kind::Mem | Kind::Commit | Kind::Swap => "MEMORY",
        Kind::Gpu => "GRAPHICS",
        Kind::Net => "NETWORK",
        Kind::Disk => "STORAGE",
    }
}

/// One watchlist line, resolved for this frame.
struct ChanView {
    id: String,
    label: String,
    kind: Kind,
    value: String,
    /// Device identity shown after the value: "NVMe SSD", "SDHC Card", a GPU
    /// model. Empty when the label already says what the instrument is.
    note: String,
    change: String,
    up: bool,
    /// No move worth colouring. A zero change painted in the "up" colour reads
    /// as a rise that is not there.
    flat: bool,
}

/// A process the user is closing. No dialog: the row itself is the trigger.
#[derive(Clone)]
struct Ticket {
    pid: u32,
    name: String,
    cpu: f32,
}

/// How long the row must be held before the order fires. Long enough that a
/// stray click cannot kill anything, short enough to feel immediate.
const HOLD_SECS: f32 = 0.6;

/// Samples kept per position. Two samples make one mini candle, and the process
/// table is re-enumerated every 2 s, so 40 samples is roughly 80 s of history.
const MINI_SAMPLES: usize = 40;

/// How long a position keeps its history after dropping out of the top list.
/// Pruning on every refresh would reset the strip of anything that briefly
/// falls below the ranking, which is most of them.
const MINI_TTL_SECS: u64 = 300;

/// Raw samples kept per logical core: 600 at 4 Hz is 150 s of instant backfill
/// when a core is pinned. About 10 KB per core.
const CORE_HIST: usize = 600;

/// Glyph for a custom title-bar window button, drawn with strokes so it
/// stays crisp at any DPI.
#[derive(Clone, Copy, PartialEq)]
enum WbIcon {
    Minimize,
    Maximize,
    Restore,
    Close,
}

#[derive(Clone)]
struct MiniPos {
    seen: Instant,
    /// CPU history. Always collected.
    q: VecDeque<f32>,
    /// Memory history. Only collected while the memory view is open, because
    /// that is the only time the sampler reads it.
    qm: VecDeque<f32>,
    /// GPU history, collected on the same terms.
    qg: VecDeque<f32>,
}

/// History for the sensor strip under the GPU chart.
#[derive(Default)]
struct GpuAux {
    vram: Series,
    temp: Series,
    power: Series,
}

/// Positions that must never be closed from here. The kernel's own processes
/// are not ordinary jobs: terminating a few of them bug-checks the machine.
fn blocked(name: &str, pid: u32) -> Option<&'static str> {
    if pid == std::process::id() {
        return Some("own position");
    }
    if pid <= 4 {
        return Some("system position");
    }
    let lower = name.to_ascii_lowercase();

    // Killing these does not crash the machine, but it takes the desktop with
    // it: the taskbar and icons vanish until the shell is started again. Too
    // easy to hit by accident, and trivially done from Task Manager when it is
    // actually intended.
    const SHELL: [&str; 2] = ["explorer.exe", "dwm.exe"];
    if SHELL.contains(&lower.as_str()) {
        return Some("system shell");
    }

    const CRITICAL: [&str; 9] = [
        "system",
        "registry",
        "smss.exe",
        "csrss.exe",
        "wininit.exe",
        "winlogon.exe",
        "services.exe",
        "lsass.exe",
        "memory compression",
    ];
    if CRITICAL.contains(&lower.as_str()) {
        return Some("system critical");
    }
    None
}

/// Width below which a dragged panel is considered closed.
const PANEL_SNAP: f32 = 48.0;
/// Height below which a dragged sensor strip is considered closed.
const STRIP_SNAP: f32 = 48.0;
const STRIP_MAX: f32 = 320.0;

/// Ends a drag at exactly zero instead of leaving a sliver that is impossible
/// to grab again.
fn snap_width(v: f32, max: f32) -> f32 {
    if v < PANEL_SNAP {
        0.0
    } else {
        v.clamp(0.0, max)
    }
}

fn snap_strip(h: f32) -> f32 {
    if h < STRIP_SNAP {
        0.0
    } else {
        h.clamp(0.0, STRIP_MAX)
    }
}

/// Chip geometry for the core strip.
///
/// The grid is derived from the panel, not fixed: eight columns was all a
/// four-core laptop needed and half of what a 32-thread Xeon needs. Chips get
/// narrower and shorter before any core is dropped, and a machine with more
/// threads than the panel can physically show gets a count of the hidden ones
/// instead of silently losing them.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CoreGrid {
    cols: usize,
    w: f32,
    h: f32,
    /// Cores actually drawn (may be less than `n` on an extreme machine).
    visible: usize,
}

const CORE_GAP: f32 = 3.0;
const CORE_MIN_H: f32 = 8.0;
const CORE_MIN_W: f32 = 14.0;

fn core_grid(n: usize, avail_w: f32, panel_h: f32) -> CoreGrid {
    let avail_w = avail_w.max(60.0);
    // Columns come in fours: 4, 8, 12, 16. Every CPU worth counting has a
    // thread count that is a multiple of four, so rows fill evenly instead of
    // ending in a ragged remainder, and a chip never gets thinner than 14 px
    // just to squeeze one more column in.
    let fit = (((avail_w + CORE_GAP) / (CORE_MIN_W + CORE_GAP)).floor() as usize).max(4);
    let cols = ((fit / 4) * 4).clamp(4, 16);
    let w = ((avail_w - CORE_GAP * (cols as f32 - 1.0)) / cols as f32)
        .floor()
        .clamp(12.0, 26.0);
    let n = n.max(1);
    let mut rows = (n + cols - 1) / cols;
    // The strip shares the panel with the position list; cells get shorter
    // rather than squeezing the list out of existence.
    let room = (panel_h - 170.0).max(48.0);
    rows = rows.min((((room + CORE_GAP) / (CORE_MIN_H + CORE_GAP)).floor() as usize).max(1));
    let h = (((room + CORE_GAP) / rows as f32) - CORE_GAP).clamp(CORE_MIN_H, 13.0);
    CoreGrid {
        cols,
        w,
        h,
        visible: n.min(rows * cols),
    }
}

/// Advances the press-and-hold state machine for one row.
///
/// Returns 0.0 while the press is too short, and 1.0 exactly once when the
/// threshold is crossed. `locked` keeps a still-held button from re-arming the
/// instant the order fires.
fn hold_progress(
    hold: &mut Option<(u32, Instant)>,
    locked: &mut bool,
    pid: u32,
    down: bool,
) -> f32 {
    if !down || *locked {
        return 0.0;
    }
    match *hold {
        Some((p, t0)) if p == pid => {
            let prog = (t0.elapsed().as_secs_f32() / HOLD_SECS).clamp(0.0, 1.0);
            if prog >= 1.0 {
                *hold = None;
                *locked = true;
            }
            prog
        }
        _ => {
            *hold = Some((pid, Instant::now()));
            0.0
        }
    }
}

/// Candle strip for one position. Two consecutive samples make one candle, so a
/// row shows the shape of the last ~80 s rather than a single number.
fn mini_chart(p: &egui::Painter, rect: Rect, samples: &VecDeque<f32>, floor: f32) {
    if samples.len() < 4 || rect.width() < 8.0 || rect.height() < 4.0 {
        return;
    }

    let v: Vec<f32> = samples.iter().copied().collect();
    let mut candles: Vec<(f32, f32, f32, f32)> = Vec::with_capacity(v.len() / 2);
    let mut i = 0;
    while i + 1 < v.len() {
        let (a, b) = (v[i], v[i + 1]);
        candles.push((a, a.max(b), a.min(b), b));
        i += 2;
    }
    if candles.is_empty() {
        return;
    }

    let lo = candles.iter().fold(f32::INFINITY, |m, c| m.min(c.2));
    let hi = candles.iter().fold(f32::NEG_INFINITY, |m, c| m.max(c.1));
    // A floor on the range: without it, a process wobbling between 0.02% and
    // 0.04% would look as dramatic as one at 90%, and a process holding 300 MB
    // would look as dramatic as one that doubled to 3 GB.
    let span = (hi - lo).max(floor);
    let (lo, hi) = (lo - span * 0.18, hi + span * 0.18);
    let y_of = |val: f32| rect.bottom() - (val - lo) / (hi - lo) * rect.height();

    // Fixed pitch, right-anchored: the newest candle pins to the right edge and
    // older ones march left at a constant spacing, so a half-filled history
    // grows in from the right instead of stretching across the whole strip.
    let slots = (MINI_SAMPLES / 2) as f32;
    let pitch = rect.width() / slots;
    let bw = (pitch * 0.6).clamp(1.5, 5.0);
    let newest = candles.len() - 1;
    for (k, (o, h, l, c)) in candles.iter().enumerate() {
        let age = (newest - k) as f32;
        let cx = rect.right() - pitch * (age + 0.5);
        let col = if c >= o { th::UP } else { th::DOWN };

        let (wt, wb) = (y_of(*h), y_of(*l));
        p.rect_filled(
            Rect::from_min_size(pos2(cx - 0.5, wt), vec2(1.0, (wb - wt).max(1.0))),
            0.0,
            col.gamma_multiply(0.65),
        );

        let top = y_of(o.max(*c));
        let bot = y_of(o.min(*c));
        p.rect_filled(
            Rect::from_min_size(pos2(cx - bw * 0.5, top), vec2(bw, (bot - top).max(1.0))),
            0.0,
            col,
        );
    }
}

/// One sensor pane under a chart. It carries the series rather than a channel
/// id, because the GPU sensors live in `gpu_aux` and the memory ones in
/// `series`.
struct CardSpec<'a> {
    key: String,
    title: &'static str,
    subtitle: String,
    dq: &'a VecDeque<Candle>,
    dropped: u64,
    unit: &'static str,
    decimals: usize,
    min_span: f32,
}

/// Draws the sensor strip with the same renderer as the main chart, so every
/// pane zooms, pans and reads out under the cursor exactly like the big one.
fn draw_cards(
    ui: &mut egui::Ui,
    rect: Rect,
    cards: &[CardSpec],
    views: &mut HashMap<String, ChartView>,
    tf: usize,
) {
    if cards.is_empty() {
        return;
    }
    let gap = 8.0;
    let n = cards.len() as f32;
    let w = (rect.width() - gap * (n - 1.0)) / n;
    for (i, card) in cards.iter().enumerate() {
        let r = Rect::from_min_size(
            pos2(rect.left() + i as f32 * (w + gap), rect.top()),
            vec2(w, rect.height()),
        );
        let view = views.entry(card.key.clone()).or_insert_with(|| {
            let mut v = ChartView::new();
            // Four pixels per candle: a card is read at a glance, and the
            // default 150-candle window would draw a handful of fat blocks on
            // a full-width pane.
            v.shown = (w / 4.0).clamp(40.0, 400.0);
            v
        });
        let opts = ChartOpts {
            title: card.title.to_string(),
            subtitle: card.subtitle.clone(),
            vol_label: String::new(),
            show_volume: false,
            compact: true,
            decimals: card.decimals,
            unit: card.unit,
            min_span: card.min_span,
            tf,
        };
        crate::chart::show(ui, r, view, card.dq, card.dropped, &opts);
    }
}

pub struct FxApp {
    sampler: sampler::Handle,
    last: Option<Snapshot>,
    /// Candle history per channel id. Instruments come and go with the
    /// hardware, so history is keyed by the stable id rather than a slot.
    series: HashMap<String, Series>,
    channels: Vec<Channel>,
    gpus: Vec<GpuStat>,
    gpu_aux: HashMap<String, GpuAux>,
    /// Zoom/pan state of each sensor pane, keyed by card. Kept per card so a
    /// zoom under the GPU chart does not follow you to the MEM page.
    card_views: HashMap<String, ChartView>,
    /// Channel the chart is following.
    selected: String,
    tf: usize,
    view: ChartView,
    started: Instant,
    fill: Option<(sampler::Fill, Instant)>,
    /// (pid, when the press started). Cleared on release.
    hold: Option<(u32, Instant)>,
    /// Set after an order fires so a still-held button cannot re-arm.
    hold_locked: bool,
    mini: HashMap<u32, MiniPos>,
    procs_gen: u64,
    /// Some(i) = the CORE instrument charts logical core i instead of the
    /// busiest one.
    core_pick: Option<usize>,
    /// Per-core raw samples, used to backfill the chart the moment a core is
    /// pinned so the switch does not land on an empty canvas.
    core_hist: Vec<VecDeque<(i64, f32, f32)>>,
    /// Same ring for the busiest-core series, so unpinning restores its history
    /// instead of starting from an empty chart.
    max_hist: VecDeque<(i64, f32, f32)>,
    /// What the position ladder measures. Mirrors the sampler's copy.
    metric: u8,
    /// Watchlist scroll offset, in pixels. Only used when the window is too
    /// short to show every instrument.
    watch_scroll: f32,
    /// Width of each side panel, in points. Zero means collapsed: the panel is
    /// still shown, at zero width, so its edge stays grabbable.
    watch_w: f32,
    procs_w: f32,
    /// Height of the sensor strip under the chart. Zero means hidden.
    strip_h: f32,
    /// The defaults depend on the window size, which is only known after the
    /// first frame.
    layout_init: bool,
    /// 0 = not fitted yet, 1 = resize requested (waiting for it to land),
    /// 2 = done. See `fit_to_work_area`.
    fit_state: u8,
    /// Undoes the caption/border winit puts back on an undecorated window
    /// where the compositor still reserves it. Dormant elsewhere.
    frame_fix: crate::win_frame::FrameFix,
    /// True while the window is borderless-fullscreen (F11).
    fullscreen: bool,
    /// Messages from the tray thread.
    tray_rx: std::sync::mpsc::Receiver<crate::tray::TrayMsg>,
    /// The tray host is up and its handle is known.
    tray_ready: bool,
    /// Tray host window handle, for show/hide pokes.
    tray_hwnd: isize,
    /// One-shot status-line note (fill reports and the like).
    note: Option<(String, Instant)>,
}

impl FxApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = th::PANEL;
        style.visuals.window_fill = th::PANEL;
        style.visuals.override_text_color = Some(th::TEXT2);
        style.spacing.item_spacing = vec2(8.0, 6.0);
        cc.egui_ctx.set_style(style);

        let sampler = sampler::spawn(250);
        sampler.set_metric(METRIC_CPU);

        // The tray host starts with the app; its icon stays hidden until
        // fullscreen takes the taskbar out of reach.
        let (tray_tx, tray_rx) = std::sync::mpsc::channel();
        crate::tray::spawn_tray(tray_tx);

        Self {
            sampler,
            last: None,
            series: HashMap::new(),
            channels: Vec::new(),
            gpus: Vec::new(),
            gpu_aux: HashMap::new(),
            card_views: HashMap::new(),
            selected: CH_CPU.to_string(),
            metric: METRIC_CPU,
            tf: 0,
            view: ChartView::new(),
            started: Instant::now(),
            fill: None,
            hold: None,
            hold_locked: false,
            mini: HashMap::new(),
            procs_gen: 0,
            // The CORE instrument follows the busiest core until a cell is
            // clicked.
            core_pick: None,
            core_hist: Vec::new(),
            max_hist: VecDeque::new(),
            watch_scroll: 0.0,
            watch_w: 198.0,
            procs_w: 214.0,
            strip_h: 160.0,
            layout_init: false,
            fit_state: 0,
            frame_fix: crate::win_frame::FrameFix::new(),
            fullscreen: false,
            tray_rx,
            tray_ready: false,
            tray_hwnd: 0,
            note: None,
        }
    }

    fn panel_frame() -> egui::Frame {
        egui::Frame::none().fill(th::PANEL)
    }

    /// Switches the chart to another instrument.
    fn select(&mut self, id: &str) {
        if self.selected == id {
            return;
        }
        self.selected = id.to_string();
        self.view.follow();
        let kind = self.channels.iter().find(|c| c.id == id).map(|c| c.kind);
        self.metric = kind.map(metric_for).unwrap_or(METRIC_CPU);
        self.sampler.set_metric(self.metric);
    }

    fn selected_kind(&self) -> Option<Kind> {
        self.channels
            .iter()
            .find(|c| c.id == self.selected)
            .map(|c| c.kind)
    }

    /// Keeps a short raw history per logical core. `Series` only stores
    /// aggregated candles, so without this, pinning a core would land on an
    /// empty chart and take a minute to become readable.
    fn remember_cores(&mut self, snap: &Snapshot) {
        if self.core_hist.len() != snap.cores.len() {
            self.core_hist.resize_with(snap.cores.len(), VecDeque::new);
        }
        for (i, v) in snap.cores.iter().enumerate() {
            let q = &mut self.core_hist[i];
            q.push_back((snap.wall, *v, snap.disk_mbps));
            while q.len() > CORE_HIST {
                q.pop_front();
            }
        }
        let peak = snap
            .channels
            .iter()
            .find(|c| c.kind == Kind::Core)
            .map(|c| c.value)
            .unwrap_or(0.0);
        self.max_hist.push_back((snap.wall, peak, snap.disk_mbps));
        while self.max_hist.len() > CORE_HIST {
            self.max_hist.pop_front();
        }
    }

    /// Switches the CORE instrument between "busiest core" and one specific
    /// core, rebuilding the series from the raw ring so the swap is instant.
    fn pin_core(&mut self, idx: Option<usize>) {
        self.core_pick = idx;
        let hist: Vec<(i64, f32, f32)> = match idx {
            Some(i) => self
                .core_hist
                .get(i)
                .map(|q| q.iter().copied().collect())
                .unwrap_or_default(),
            None => self.max_hist.iter().copied().collect(),
        };
        if let Some(s) = self.series.get_mut(CH_CORE) {
            s.clear();
            for (t, v, flow) in hist {
                s.push(t, v, flow);
            }
        }
        self.view.follow();
    }

    /// The tray icon exists only while fullscreen keeps the taskbar out of
    /// reach, and hides otherwise.
    fn sync_tray(&self) {
        crate::tray::notify_visible(self.tray_hwnd, self.fullscreen);
    }

    /// One window-control cell in the custom title bar: a full-height cell,
    /// hover fills the cell (red for close, like the native frame), the icon
    /// is drawn with strokes. Returns whether it was clicked.
    fn bar_button(ui: &egui::Ui, rect: egui::Rect, id: &str, icon: WbIcon, enabled: bool) -> bool {
        let resp = ui.interact(rect, egui::Id::new(id), egui::Sense::click());
        let mut fill = th::BG;
        if enabled && resp.hovered() {
            fill = if matches!(icon, WbIcon::Close) { th::UP } else { th::SURFACE };
        }
        if enabled && resp.is_pointer_button_down_on() {
            fill = fill.gamma_multiply(0.65);
        }
        let p = ui.painter();
        p.rect_filled(rect, 0.0, fill);

        let col = if enabled { th::TEXT2 } else { th::DIM };
        let c = rect.center();
        let st = egui::Stroke::new(1.3_f32, col);
        match icon {
            WbIcon::Minimize => {
                p.line_segment([pos2(c.x - 5.0, c.y + 2.5), pos2(c.x + 5.0, c.y + 2.5)], st);
            }
            WbIcon::Maximize => {
                p.rect_stroke(Rect::from_center_size(c, vec2(10.0, 10.0)), 0.0, st);
            }
            WbIcon::Restore => {
                let back = Rect::from_center_size(pos2(c.x + 1.5, c.y - 1.5), vec2(8.0, 8.0));
                let front = Rect::from_center_size(pos2(c.x - 1.5, c.y + 1.5), vec2(8.0, 8.0));
                p.rect_stroke(back, 0.0, st);
                // The front square is filled with the cell's own background,
                // which is what cuts the overlap out of the back square.
                p.rect_filled(front, 0.0, fill);
                p.rect_stroke(front, 0.0, st);
            }
            WbIcon::Close => {
                p.line_segment([pos2(c.x - 4.5, c.y - 4.5), pos2(c.x + 4.5, c.y + 4.5)], st);
                p.line_segment([pos2(c.x - 4.5, c.y + 4.5), pos2(c.x + 4.5, c.y - 4.5)], st);
            }
        }
        enabled && resp.clicked()
    }

    /// The window opens at a design size that assumes a roomy screen. On a
    /// smaller monitor — or a laptop at 150 %, where 1240x780 points is
    /// 1860x1170 physical pixels — that request does not fit, and Windows
    /// clamps the window to the *screen* rather than the work area, which
    /// leaves the status bar sitting under the taskbar with no way to see it.
    ///
    /// So the first frame measures the work area (taskbar already excluded)
    /// and asks for a size that fits, with a small margin. After that the
    /// geometry belongs to the user: a later drag is not undone.
    fn fit_to_work_area(&mut self, ctx: &egui::Context) {
        const MARGIN: f32 = 12.0;
        const FITTING: u8 = 1;
        const DONE: u8 = 2;

        match self.fit_state {
            0 => {
                self.fit_state = DONE;
                let Some((ax, ay, aw, ah)) =
                    crate::win_sensors::work_area_points(ctx.pixels_per_point())
                else {
                    return;
                };
                let size = ctx.screen_rect().size();
                let max = vec2(aw - MARGIN * 2.0, ah - MARGIN * 2.0);
                let fitted = vec2(size.x.min(max.x), size.y.min(max.y));
                let resized = (size.x - fitted.x).abs() > 0.5 || (size.y - fitted.y).abs() > 0.5;

                // Shrinking alone is not enough: the window may have opened
                // low on the screen, in which case its bottom still hangs off
                // the work area. Clamp the top-left corner to keep the whole
                // frame inside.
                let outer = ctx.input(|i| i.viewport().outer_rect);
                let cur = outer
                    .map(|r| r.min)
                    .unwrap_or(pos2(ax + MARGIN, ay + MARGIN));
                let lo = pos2(ax + MARGIN, ay + MARGIN);
                let hi = pos2(
                    (ax + aw - fitted.x).max(lo.x),
                    (ay + ah - fitted.y).max(lo.y),
                );
                let target = pos2(cur.x.clamp(lo.x, hi.x), cur.y.clamp(lo.y, hi.y));
                let moved = (cur - target).length() > 0.5;

                if resized || moved {
                    self.fit_state = FITTING;
                    if resized {
                        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(fitted));
                        // Panel defaults are seeded from the window size; seed
                        // them against the size the window is about to have.
                        self.layout_init = false;
                    }
                    if moved {
                        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(target));
                    }
                }
            }
            FITTING => self.fit_state = DONE,
            _ => {}
        }
    }

    fn header(&mut self, ctx: &egui::Context) {
        // The OS title bar is gone — this bar *is* the title bar now, the
        // way Task Manager and VS Code draw their own. Drag it to move the
        // window (the native loop gives Aero snap for free), double-click
        // it to maximize, and the three cells at the right edge are the
        // window buttons.
        egui::TopBottomPanel::top("fx_header")
            .exact_height(42.0)
            .frame(Self::panel_frame())
            .show(ctx, |ui| {
                let rect = ui.available_rect_before_wrap();
                let p = ui.painter();
                let cy = rect.center().y;

                // The bar reads as its own strip: a notch darker than the
                // panels, with a hairline underneath.
                p.rect_filled(rect, 0.0, th::BG);
                p.line_segment(
                    [
                        pos2(rect.left(), rect.bottom() - 0.5),
                        pos2(rect.right(), rect.bottom() - 0.5),
                    ],
                    egui::Stroke::new(1.0_f32, th::LINE),
                );

                // The whole bar is draggable and double-clickable. The chips
                // and window buttons register their own interactions on top
                // of this one, so they win wherever they are.
                let bar = ui.interact(rect, egui::Id::new("fx_titlebar"), egui::Sense::click_and_drag());
                if !self.fullscreen && bar.drag_started_by(egui::PointerButton::Primary) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if !self.fullscreen && bar.double_clicked_by(egui::PointerButton::Primary) {
                    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }

                let r = p.text(
                    pos2(rect.left() + 16.0, cy),
                    Align2::LEFT_CENTER,
                    "FXVIEWER",
                    FontId::monospace(14.0),
                    th::TEXT,
                );
                if rect.width() >= 760.0 {
                    p.text(
                        pos2(r.right() + 10.0, cy),
                        Align2::LEFT_CENTER,
                        "resource terminal",
                        FontId::proportional(10.0),
                        th::DIM,
                    );
                    p.text(
                        pos2(r.right() + 116.0, cy),
                        Align2::LEFT_CENTER,
                        "v0.2",
                        FontId::monospace(9.5),
                        th::DIM,
                    );
                }

                // Window buttons, flush against the right edge: – □ ✕, each
                // a full-height cell. Maximize and minimize stay dormant in
                // fullscreen, where both would fight the viewport state.
                let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                let bw = 40.0;
                let top = rect.top();
                let bot = rect.bottom();
                let close_r = Rect::from_min_max(pos2(rect.right() - bw, top), pos2(rect.right(), bot));
                let max_r = Rect::from_min_max(pos2(close_r.left() - bw, top), pos2(close_r.left(), bot));
                let min_r = Rect::from_min_max(pos2(max_r.left() - bw, top), pos2(max_r.left(), bot));

                if Self::bar_button(ui, min_r, "win_min", WbIcon::Minimize, !self.fullscreen) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
                let max_icon = if maximized { WbIcon::Restore } else { WbIcon::Maximize };
                if Self::bar_button(ui, max_r, "win_max", max_icon, !self.fullscreen) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
                if Self::bar_button(ui, close_r, "win_close", WbIcon::Close, true) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }

                let mut x = min_r.left() - 16.0;
                p.text(
                    pos2(x - TIMEFRAMES.len() as f32 * 36.0 - 16.0, cy),
                    Align2::RIGHT_CENTER,
                    util::clock_now().as_str(),
                    FontId::monospace(10.0),
                    th::MUTED,
                );
                for i in (0..TIMEFRAMES.len()).rev() {
                    let chip = Rect::from_min_size(pos2(x - 34.0, cy - 11.0), vec2(34.0, 22.0));
                    let resp = ui.interact(chip, egui::Id::new(("tf", i)), Sense::click());
                    let active = self.tf == i;
                    if active {
                        ui.painter().rect_filled(chip, 3.0, th::SURFACE);
                    }
                    ui.painter().text(
                        chip.center(),
                        Align2::CENTER_CENTER,
                        TF_LABELS[i],
                        FontId::monospace(10.0),
                        if active { th::TEXT } else { th::MUTED },
                    );
                    if resp.clicked() {
                        self.tf = i;
                        self.view.follow();
                    }
                    x -= 36.0;
                }
            });
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        let (self_cpu, self_mem) = self
            .last
            .as_ref()
            .map(|s| (s.self_cpu, s.self_mem))
            .unwrap_or((0.0, 0.0));
        let elapsed = self.started.elapsed().as_secs_f64();

        // While fullscreen the taskbar is out of reach, so the way back takes
        // the hint slot permanently — on any width.
        let fs_hint = if self.fullscreen {
            Some("fullscreen · F11 or tray to exit".to_string())
        } else {
            None
        };
        let vol_hint = if fs_hint.is_some() {
            String::new()
        } else if ctx.screen_rect().width() < 880.0 {
            String::new()
        } else {
            match self.selected_kind() {
                Some(Kind::Net) => "volume = this adapter".to_string(),
                Some(Kind::Disk) => "volume = this volume".to_string(),
                _ => "volume = disk total".to_string(),
            }
        };

        egui::TopBottomPanel::bottom("fx_status")
            .exact_height(28.0)
            .frame(Self::panel_frame())
            .show(ctx, |ui| {
                let rect = ui.available_rect_before_wrap();
                let p = ui.painter();
                let cy = rect.center().y;

                // The right-hand block is anchored to the edge and drawn
                // first: everything on the left is optional, and on a quarter
                // tile it has to give way rather than run underneath it.
                let uptime = util::uptime(elapsed);
                let right = p.text(
                    pos2(rect.right() - 16.0, cy),
                    Align2::RIGHT_CENTER,
                    uptime.as_str(),
                    FontId::monospace(9.5),
                    th::DIM,
                );
                let self_text = p.text(
                    pos2(right.left() - 16.0, cy),
                    Align2::RIGHT_CENTER,
                    format!(
                        "self {}% cpu · {:.0} mb",
                        util::fmt_small_pct(self_cpu),
                        self_mem
                    )
                    .as_str(),
                    FontId::monospace(9.5),
                    th::DOWN,
                );
                let limit = self_text.left() - 14.0;

                p.circle_filled(pos2(rect.left() + 16.0, cy), 3.0, th::DOWN);
                let mut r = p.text(
                    pos2(rect.left() + 26.0, cy),
                    Align2::LEFT_CENTER,
                    "live · 4 hz sampling",
                    FontId::monospace(9.5),
                    th::MUTED,
                );
                if r.right() < limit {
                    r = p.text(
                        pos2(r.right() + 16.0, cy),
                        Align2::LEFT_CENTER,
                        format!("{} candles · buffer {}", TF_LABELS[self.tf], crate::series::HISTORY)
                            .as_str(),
                        FontId::monospace(9.5),
                        th::DIM,
                    );
                }
                let mut hint: Option<egui::Rect> = None;
                let slot = fs_hint
                    .clone()
                    .or_else(|| (!vol_hint.is_empty()).then(|| vol_hint.clone()));
                let force = fs_hint.is_some();
                if let Some(txt) = slot {
                    if r.right() < limit - if force { 0.0 } else { 80.0 } {
                        let hr = p.text(
                            pos2(r.right() + 16.0, cy),
                            Align2::LEFT_CENTER,
                            txt.as_str(),
                            FontId::proportional(9.5),
                            th::DIM,
                        );
                        hint = Some(hr);
                    }
                }

                // The transient line: a pin note or a fill report, whichever
                // is newer. Both fade out after a few seconds.
                let now = Instant::now();
                let note_live = self
                    .note
                    .as_ref()
                    .filter(|(_, at)| now.duration_since(*at).as_secs_f32() < 8.0);
                let fill_live = if note_live.is_some() {
                    None
                } else {
                    self.fill
                        .as_ref()
                        .filter(|(_, at)| now.duration_since(*at).as_secs_f32() < 8.0)
                };
                let transient = match (note_live, fill_live) {
                    (Some((txt, at)), _) => {
                        let secs = now.duration_since(*at).as_secs_f32();
                        let fade = (1.0 - (secs - 6.0).max(0.0) / 2.0).clamp(0.0, 1.0);
                        Some((txt.clone(), th::TEXT, fade))
                    }
                    (None, Some((f, at))) => {
                        let secs = now.duration_since(*at).as_secs_f32();
                        let fade = (1.0 - (secs - 6.0).max(0.0) / 2.0).clamp(0.0, 1.0);
                        let (txt, col) = if f.ok {
                            (
                                format!(
                                    "FILLED · {} [{}] · {}% released",
                                    shorten(&f.name, 14),
                                    f.pid,
                                    util::fmt_cpu_share(f.cpu)
                                ),
                                th::DOWN,
                            )
                        } else {
                            (
                                format!(
                                    "REJECTED · {} [{}] · {}",
                                    shorten(&f.name, 14),
                                    f.pid,
                                    f.error.unwrap_or("failed")
                                ),
                                th::UP,
                            )
                        };
                        Some((txt, col, fade))
                    }
                    (None, None) => None,
                };
                if let Some((txt, col, fade)) = transient {
                    if let Some(h) = hint {
                        if h.right() < limit {
                            p.text(
                                pos2(h.right() + 18.0, cy),
                                Align2::LEFT_CENTER,
                                txt.as_str(),
                                FontId::monospace(9.5),
                                col.gamma_multiply(fade),
                            );
                        }
                    }
                }
            });
    }

    /// Resolves the watchlist for this frame: every channel, its last close and
    /// the move since the previous candle.
    fn watch_rows(&self) -> Vec<ChanView> {
        self.channels
            .iter()
            // COMMIT and SWAP are drawn as sensor panes under the MEM chart;
            // keeping them in the list as well would be the same numbers twice.
            .filter(|ch| !matches!(ch.kind, Kind::Commit | Kind::Swap))
            .map(|ch| {
                let s = self.series.get(&ch.id);
                let last = s.and_then(|s| s.last(0)).map(|c| c.c);
                let prev = s.and_then(|s| s.prev_close(0));
                let (change, up, flat) = match (last, prev) {
                    (Some(l), Some(p)) => {
                        let flat = (l - p).abs() < if ch.unit == "%" { 0.05 } else { 0.005 };
                        if ch.unit == "%" {
                            (fmt_change(l, p), l >= p, flat)
                        } else {
                            // Absolute delta: see `fmt_delta_rate`.
                            (fmt_delta_rate(p, l), l >= p, flat)
                        }
                    }
                    _ => ("—".to_string(), true, true),
                };
                ChanView {
                    id: ch.id.clone(),
                    label: ch.label.clone(),
                    kind: ch.kind,
                    value: fmt_value(ch.unit, ch.total, ch.dec, last.unwrap_or(0.0)),
                    note: ch.note.clone(),
                    change,
                    up,
                    flat,
                }
            })
            .collect()
    }

    fn watchlist(&mut self, ctx: &egui::Context) {
        let rows = self.watch_rows();
        // Side panels give way on a narrow window: in a quarter tile the chart
        // is the thing being read, and 18 px per panel buys it 36 px of plot.
        let narrow = ctx.screen_rect().width() < 880.0;
        // A dragged-wide panel must not eat the chart when the window is then
        // shrunk: the maximum follows the window instead of being absolute.
        let left_max = (ctx.screen_rect().width() * 0.32).clamp(180.0, 340.0);
        let mut pick: Option<String> = None;
        let mut scroll = self.watch_scroll;

        // Seeded here because the window size is only known once a frame has
        // been laid out — and not while the opening window is still being
        // fitted, or the defaults would be seeded from the oversized one.
        if !self.layout_init && self.fit_state != 1 {
            self.watch_w = if narrow { 172.0 } else { 198.0 };
            self.procs_w = if narrow { 184.0 } else { 214.0 };
            // A short window gets no sensor strip by default; the tab at the
            // bottom edge brings it back.
            self.strip_h = if ctx.screen_rect().height() > 560.0 {
                160.0
            } else {
                0.0
            };
            self.layout_init = true;
        }
        self.watch_w = self.watch_w.clamp(0.0, left_max);

        egui::SidePanel::left("fx_watch")
            // The width is ours, not egui's: a panel that egui resizes cannot
            // go to zero and come back, and zero is the point.
            .exact_width(self.watch_w)
            .frame(Self::panel_frame())
            .show(ctx, |ui| {
                let rect = ui.available_rect_before_wrap();
                if rect.width() < 8.0 {
                    return;
                }
                ui.set_min_size(rect.size());
                let mut y = rect.top() + 10.0;

                ui.painter().text(
                    pos2(rect.left() + 14.0, y),
                    Align2::LEFT_TOP,
                    "WATCHLIST",
                    FontId::proportional(9.5),
                    th::DIM,
                );
                y += 18.0;

                const ROW_H: f32 = 38.0;
                const HEAD_H: f32 = 15.0;
                const INST_H: f32 = 34.0 + 5.0 * 17.0;

                let list_top = y;
                // In a quarter tile the list is the only thing worth the
                // pixels: the machine summary is five lines of trivia that
                // would otherwise cost three instruments of visible space.
                let show_inst = rect.height() > 520.0;
                let inst_h = if show_inst { INST_H } else { 0.0 };
                let list_bottom = (rect.bottom() - inst_h).max(list_top + 60.0);
                let mut content = 0.0;
                let mut group = "";
                for r in &rows {
                    let g = group_of(r.kind);
                    if g != group {
                        content += HEAD_H;
                        group = g;
                    }
                    content += ROW_H;
                }
                let max_scroll = (content - (list_bottom - list_top)).max(0.0);
                scroll = scroll.clamp(0.0, max_scroll);

                let list_rect =
                    Rect::from_min_max(pos2(rect.left(), list_top), pos2(rect.right(), list_bottom));
                let hovered = ui
                    .input(|i| i.pointer.hover_pos())
                    .map_or(false, |p| list_rect.contains(p));
                if hovered {
                    let d = ui.input(|i| i.smooth_scroll_delta.y);
                    if d.abs() > 0.1 {
                        scroll = (scroll - d).clamp(0.0, max_scroll);
                    }
                }

                let p = ui.painter().with_clip_rect(list_rect);
                let mut ry = list_top - scroll;
                let mut drawn_group = "";
                for r in &rows {
                    let g = group_of(r.kind);
                    if g != drawn_group {
                        // A heading whose rows are all below the fold is worse
                        // than no heading: it reads like the section is empty.
                        if ry + HEAD_H + ROW_H > list_bottom {
                            break;
                        }
                        if ry + HEAD_H > list_top {
                            p.text(
                                pos2(rect.left() + 14.0, ry + 3.0),
                                Align2::LEFT_TOP,
                                g,
                                FontId::proportional(8.5),
                                th::DIM,
                            );
                        }
                        ry += HEAD_H;
                        drawn_group = g;
                    }
                    if ry + ROW_H > list_top && ry < list_bottom {
                        let row = Rect::from_min_size(
                            pos2(rect.left() + 6.0, ry),
                            vec2(rect.width() - 12.0, ROW_H - 2.0),
                        );
                        let hit = row.intersect(list_rect);
                        let resp = ui
                            .interact(hit, egui::Id::new(("sym", &r.id)), Sense::click())
                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                        let selected = self.selected == r.id;
                        if selected {
                            ui.painter().rect_filled(row, 4.0, th::SURFACE);
                            ui.painter().rect_filled(
                                Rect::from_min_size(
                                    pos2(row.left(), row.top() + 7.0),
                                    vec2(2.0, row.height() - 14.0),
                                ),
                                1.0,
                                th::MA20,
                            );
                        } else if resp.hovered() {
                            ui.painter().rect_filled(row, 4.0, th::SURFACE);
                        }
                        p.text(
                            pos2(row.left() + 10.0, row.top() + 7.0),
                            Align2::LEFT_TOP,
                            r.label.as_str(),
                            FontId::proportional(12.0),
                            if selected { th::TEXT } else { th::TEXT2 },
                        );
                        p.text(
                            pos2(row.right() - 8.0, row.top() + 8.0),
                            Align2::RIGHT_TOP,
                            r.change.as_str(),
                            FontId::monospace(9.5),
                            if r.flat {
                                th::DIM
                            } else if r.up {
                                th::UP
                            } else {
                                th::DOWN
                            },
                        );
                        let vr = p.text(
                            pos2(row.left() + 10.0, row.top() + 22.0),
                            Align2::LEFT_TOP,
                            r.value.as_str(),
                            FontId::monospace(9.5),
                            th::MUTED,
                        );
                        // The note shares the second line with the change
                        // column. Measure both rather than guessing at a
                        // character width: a dragged-narrow panel then loses
                        // only as much of the note as it has to.
                        if !r.note.is_empty() {
                            let change_w = p
                                .layout_no_wrap(
                                    r.change.clone(),
                                    FontId::monospace(9.5),
                                    th::DIM,
                                )
                                .size()
                                .x;
                            let room =
                                row.right() - 8.0 - change_w - 8.0 - (vr.right() + 5.0);
                            if room > 18.0 {
                                let full = format!("· {}", r.note);
                                let w = p
                                    .layout_no_wrap(
                                        full.clone(),
                                        FontId::proportional(9.0),
                                        th::DIM,
                                    )
                                    .size()
                                    .x;
                                // Prefer the separator, then the bare note,
                                // and only then truncate: a value that grows a
                                // digit should not make the note vanish.
                                let bare_w = p
                                    .layout_no_wrap(
                                        r.note.clone(),
                                        FontId::proportional(9.0),
                                        th::DIM,
                                    )
                                    .size()
                                    .x;
                                let text = if w <= room {
                                    full
                                } else if bare_w <= room {
                                    r.note.clone()
                                } else {
                                    let keep = ((full.chars().count() as f32 * room / w)
                                        .floor() as usize)
                                        .max(2);
                                    shorten(&full, keep)
                                };
                                p.text(
                                    pos2(vr.right() + 5.0, row.top() + 23.0),
                                    Align2::LEFT_TOP,
                                    text.as_str(),
                                    FontId::proportional(9.0),
                                    th::DIM,
                                );
                            }
                        }
                        if resp.clicked() {
                            pick = Some(r.id.clone());
                        }
                    }
                    ry += ROW_H;
                }

                // Scroll hint: without it a clipped list looks like a list that
                // is missing its last instruments.
                if max_scroll > 0.0 {
                    let track = Rect::from_min_max(
                        pos2(rect.right() - 6.0, list_top + 2.0),
                        pos2(rect.right() - 3.0, list_bottom - 2.0),
                    );
                    ui.painter().rect_filled(track, 1.0, th::LINE);
                    let frac = (list_bottom - list_top) / content;
                    let h = (track.height() * frac).clamp(16.0, track.height());
                    let t = (scroll / max_scroll) * (track.height() - h);
                    ui.painter().rect_filled(
                        Rect::from_min_size(pos2(track.left(), track.top() + t), vec2(3.0, h)),
                        1.5,
                        th::MUTED,
                    );
                }

                // ---- instrument summary, anchored to the bottom edge -------
                if show_inst {
                    let block_top = rect.bottom() - INST_H;
                    if block_top > list_top + 24.0 {
                        ui.painter().hline(
                            (rect.left() + 14.0)..=(rect.right() - 14.0),
                            block_top - 4.0,
                            egui::Stroke::new(1.0_f32, th::LINE),
                        );
                    }
                    let mut by = block_top + 6.0;
                    ui.painter().text(
                        pos2(rect.left() + 14.0, by),
                        Align2::LEFT_TOP,
                        "INSTRUMENT",
                        FontId::proportional(9.5),
                        th::DIM,
                    );
                    by += 18.0;

                    let snap = self.last.as_ref();
                    let info: [(&str, String); 5] = [
                        (
                            "cpu",
                            snap.map(|s| shorten(&s.cpu_brand, 24))
                                .unwrap_or_else(|| "—".to_string()),
                        ),
                        (
                            "cores",
                            snap.map(|s| s.ncores.to_string())
                                .unwrap_or_else(|| "—".to_string()),
                        ),
                        (
                            "memory",
                            snap.map(|s| format!("{:.1} / {:.1} gb", s.used_mem_gb, s.total_mem_gb))
                                .unwrap_or_else(|| "—".to_string()),
                        ),
                        (
                            "host",
                            snap.map(|s| shorten(&s.host, 22))
                                .unwrap_or_else(|| "—".to_string()),
                        ),
                        (
                            "os",
                            snap.map(|s| shorten(&s.os, 22))
                                .unwrap_or_else(|| "—".to_string()),
                        ),
                    ];
                    for (k, v) in info {
                        let p = ui.painter();
                        p.text(
                            pos2(rect.left() + 14.0, by),
                            Align2::LEFT_TOP,
                            k,
                            FontId::proportional(10.0),
                            th::DIM,
                        );
                        p.text(
                            pos2(rect.left() + 62.0, by),
                            Align2::LEFT_TOP,
                            v.as_str(),
                            FontId::proportional(10.0),
                            th::MUTED,
                        );
                        by += 17.0;
                    }
                }
            });

        self.watch_scroll = scroll;
        if let Some(id) = pick {
            self.select(&id);
        }
    }

    fn processes(&mut self, ctx: &egui::Context) {
        let mut fire: Option<Ticket> = None;
        let mut picked: Option<usize> = None;
        let mut hold = self.hold;
        let mut locked = self.hold_locked;

        let procs = self
            .last
            .as_ref()
            .map(|s| s.procs.clone())
            .unwrap_or_default();
        let cores = self
            .last
            .as_ref()
            .map(|s| s.cores.clone())
            .unwrap_or_default();
        let mini = self.mini.clone();
        let mem_view = self.metric == METRIC_MEM;
        let gpu_view = self.metric == METRIC_GPU;

        let right_max = (ctx.screen_rect().width() * 0.34).clamp(180.0, 360.0);
        self.procs_w = self.procs_w.clamp(0.0, right_max);

        egui::SidePanel::right("fx_procs")
            .exact_width(self.procs_w)
            .frame(Self::panel_frame())
            .show(ctx, |ui| {
                let rect = ui.available_rect_before_wrap();
                if rect.width() < 8.0 {
                    return;
                }
                ui.set_min_size(rect.size());

                // The cores block is anchored to the bottom edge, so a tall
                // window does not end in a large empty gap under the list.
                let grid = core_grid(cores.len(), rect.width() - 28.0, rect.height());
                let (cols, cw, ch, cgap) = (grid.cols, grid.w, grid.h, CORE_GAP);
                let rows = (grid.visible + cols - 1) / cols;
                let block_h = rows as f32 * (ch + cgap) - cgap;
                // The core strip follows the list instead of hugging the bottom
                // edge: on an idle machine only a handful of processes clear the
                // noise floor, and a bottom-anchored block would leave a hole in
                // the middle of the panel.
                let limit_y = rect.bottom() - 76.0 - block_h;

                if !ui.input(|i| i.pointer.primary_down()) {
                    hold = None;
                    locked = false;
                }

                let mut y = rect.top() + 10.0;
                let p = ui.painter();

                p.text(
                    pos2(rect.left() + 14.0, y),
                    Align2::LEFT_TOP,
                    "OPEN POSITIONS",
                    FontId::proportional(9.5),
                    th::DIM,
                );
                // On a panel dragged narrow the two headings would collide;
                // the interaction is discoverable from the subtitle alone.
                if rect.width() >= 178.0 {
                    p.text(
                        pos2(rect.right() - 14.0, y + 1.0),
                        Align2::RIGHT_TOP,
                        "HOLD TO CLOSE",
                        FontId::proportional(8.5),
                        th::DIM,
                    );
                }
                p.text(
                    pos2(rect.left() + 14.0, y + 13.0),
                    Align2::LEFT_TOP,
                    if mem_view {
                        "value = resident memory"
                    } else if gpu_view {
                        "value = gpu engine"
                    } else {
                        "value = 8 s average"
                    },
                    FontId::proportional(8.5),
                    th::DIM,
                );
                y += 30.0;

                let row_h = 40.0;

                for (idx, row) in procs.iter().enumerate() {
                    if y + row_h > limit_y {
                        break;
                    }

                    // The row itself is the trigger: press and hold to close,
                    // release early to abort. No dialog, no mouse travel.
                    let hit = Rect::from_min_size(
                        pos2(rect.left() + 8.0, y - 2.0),
                        vec2(rect.width() - 16.0, row_h - 4.0),
                    );
                    let resp = ui
                        .interact(hit, egui::Id::new(("pos", row.pid)), Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);

                    let prog = hold_progress(
                        &mut hold,
                        &mut locked,
                        row.pid,
                        resp.is_pointer_button_down_on(),
                    );
                    if prog >= 1.0 {
                        fire = Some(Ticket {
                            pid: row.pid,
                            name: row.name.clone(),
                            cpu: row.avg,
                        });
                    }

                    if prog > 0.0 {
                        let w = hit.width() * prog;
                        ui.painter().rect_filled(
                            Rect::from_min_size(hit.min, vec2(w, hit.height())),
                            3.0,
                            th::UP.gamma_multiply(0.18),
                        );
                    } else if resp.hovered() {
                        ui.painter().rect_filled(hit, 3.0, th::SURFACE);
                    }

                    let p = ui.painter();
                    p.text(
                        pos2(rect.left() + 14.0, y + 3.0),
                        Align2::LEFT_TOP,
                        shorten(&row.name, 17).as_str(),
                        FontId::proportional(10.5),
                        if idx == 0 { th::TEXT2 } else { th::MUTED },
                    );
                    let shown = if mem_view {
                        util::fmt_mb(row.mem_mb)
                    } else if gpu_view {
                        util::fmt_cpu_share(row.gpu_pct)
                    } else {
                        util::fmt_cpu_share(row.avg)
                    };
                    p.text(
                        pos2(rect.right() - 14.0, y + 3.0),
                        Align2::RIGHT_TOP,
                        shown.as_str(),
                        FontId::monospace(10.0),
                        if prog > 0.0 { th::UP } else { th::TEXT2 },
                    );

                    let strip = Rect::from_min_size(
                        pos2(rect.left() + 14.0, y + 18.0),
                        vec2(rect.width() - 28.0, 15.0),
                    );
                    if let Some(pos) = mini.get(&row.pid) {
                        if mem_view {
                            mini_chart(ui.painter(), strip, &pos.qm, 8.0);
                        } else if gpu_view {
                            mini_chart(ui.painter(), strip, &pos.qg, 2.0);
                        } else {
                            mini_chart(ui.painter(), strip, &pos.q, 1.0);
                        }
                    }
                    y += row_h;
                }

                let label_y = (y + 6.0).min(rect.bottom() - 24.0 - block_h);
                let grid_top = label_y + 18.0;
                let caption_y = grid_top + block_h + 10.0;

                let p = ui.painter();
                if y + 8.0 < label_y - 8.0 {
                    p.hline(
                        (rect.left() + 14.0)..=(rect.right() - 14.0),
                        label_y - 8.0,
                        egui::Stroke::new(1.0_f32, th::LINE),
                    );
                }
                p.text(
                    pos2(rect.left() + 14.0, label_y),
                    Align2::LEFT_TOP,
                    "CORES",
                    FontId::proportional(9.5),
                    th::DIM,
                );

                for (i, load) in cores.iter().take(grid.visible).enumerate() {
                    let cx = rect.left() + 14.0 + (i % cols) as f32 * (cw + cgap);
                    let cyy = grid_top + (i / cols) as f32 * (ch + cgap);
                    let cell = Rect::from_min_size(pos2(cx, cyy), vec2(cw, ch));
                    let resp = ui
                        .interact(cell, egui::Id::new(("core", i)), Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.clicked() {
                        picked = Some(i);
                    }

                    // The pinned core gets a light halo, so you can tell which
                    // one the CORE chart is following.
                    if self.core_pick == Some(i) {
                        ui.painter().rect_filled(cell.expand(1.5), 3.0, th::TEXT2);
                    }
                    ui.painter().rect_filled(
                        cell,
                        2.0,
                        th::load_level(*load).gamma_multiply(0.9),
                    );
                    ui.painter().text(
                        pos2(cx + cw * 0.5, cyy + ch * 0.5),
                        Align2::CENTER_CENTER,
                        format!("{:.0}", load),
                        FontId::monospace(if ch < 11.0 {
                            7.5
                        } else if cw < 18.0 {
                            8.0
                        } else {
                            8.5
                        }),
                        th::BG,
                    );
                }

                let p = ui.painter();
                p.text(
                    pos2(rect.left() + 14.0, caption_y),
                    Align2::LEFT_TOP,
                    if grid.visible < cores.len() {
                        format!(
                            "{} logical cores · {} hidden",
                            cores.len(),
                            cores.len() - grid.visible
                        )
                    } else {
                        format!("{} logical cores", cores.len())
                    }
                    .as_str(),
                    FontId::proportional(9.5),
                    th::DIM,
                );
            });

        self.hold = hold;
        self.hold_locked = locked;

        if let Some(i) = picked {
            let next = if self.core_pick == Some(i) { None } else { Some(i) };
            self.pin_core(next);
        }

        if let Some(t) = fire {
            match blocked(&t.name, t.pid) {
                Some(reason) => {
                    self.fill = Some((
                        sampler::Fill {
                            pid: t.pid,
                            name: t.name,
                            cpu: t.cpu,
                            ok: false,
                            error: Some(reason),
                        },
                        Instant::now(),
                    ));
                }
                None => self.sampler.close(t.pid),
            }
        }
    }

    fn chart(&mut self, ctx: &egui::Context) {
        // Taken out for the duration of the frame: the strip needs `&mut`
        // ChartView while the rest of the page is borrowed immutably.
        let mut card_views = std::mem::take(&mut self.card_views);
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(th::BG))
            .show(ctx, |ui| {
                let full = ui.available_rect_before_wrap();
                let Some(ch) = self.channels.iter().find(|c| c.id == self.selected) else {
                    ui.painter().text(
                        full.center(),
                        Align2::CENTER_CENTER,
                        "starting sampler…",
                        FontId::proportional(12.0),
                        th::MUTED,
                    );
                    return;
                };

                let (title, subtitle) = match (ch.kind, self.core_pick) {
                    (Kind::Core, Some(i)) => (
                        format!("CORE {:02}", i + 1),
                        "pinned logical core".to_string(),
                    ),
                    _ => (ch.label.clone(), ch.desc.clone()),
                };
                let vol_label = match ch.kind {
                    Kind::Net => "VOL · this adapter MB/s",
                    Kind::Disk => "VOL · this volume MB/s",
                    _ => "VOL · disk total MB/s",
                };
                // Rate instruments measure their own volume, so the pane would
                // just redraw the candles underneath them.
                let show_volume = !matches!(ch.kind, Kind::Net | Kind::Disk);

                // GPU pages carry sensor candles underneath; so does MEM now,
                // which is where SWAP lives.
                let gpu = if ch.kind == Kind::Gpu {
                    self.gpus.iter().find(|g| Some(g.id.as_str()) == ch.id.strip_prefix("gpu:"))
                } else {
                    None
                };
                let has_strip = matches!(ch.kind, Kind::Gpu | Kind::Mem);
                // The strip is the user's to size: drag its top edge down to
                // nothing and it goes away, drag it back up and it returns.
                // Up to 55 % of the pane: enough for a readable card at
                // quarter-screen size, and still the user's call.
                let max_strip = (full.height() * 0.55).clamp(0.0, STRIP_MAX);
                let want_strip = self.strip_h.min(max_strip);
                let can_strip = has_strip && full.width() > 380.0;
                let with_strip = can_strip && want_strip >= 48.0;
                let strip_h = if with_strip { want_strip } else { 0.0 };
                let (main, strip) = if with_strip {
                    (
                        Rect::from_min_max(
                            full.min,
                            pos2(full.right(), full.bottom() - strip_h - 8.0),
                        ),
                        Some(Rect::from_min_max(
                            pos2(full.left(), full.bottom() - strip_h),
                            full.max,
                        )),
                    )
                } else {
                    (full, None)
                };

                // An idle instrument should not zoom into its own jitter: the
                // floor is in the instrument's own unit (0.5 GB is a readable
                // slice of memory, 1e-3 would draw noise as a mountain range).
                let min_span = match ch.kind {
                    Kind::Mem | Kind::Commit => 0.5,
                    Kind::Swap => 0.05,
                    _ => 0.0,
                };
                let opts = ChartOpts {
                    title,
                    subtitle,
                    vol_label: vol_label.to_string(),
                    show_volume,
                    decimals: ch.dec,
                    unit: ch.unit,
                    min_span,
                    compact: false,
                    tf: self.tf,
                };

                let series = self.series.get(&ch.id);
                let empty = VecDeque::new();
                let dq = series.map(|s| s.candles(self.tf)).unwrap_or(&empty);
                let dropped = series.map(|s| s.dropped(self.tf)).unwrap_or(0);
                crate::chart::show(ui, main, &mut self.view, dq, dropped, &opts);

                if let Some(rect) = strip {
                    let mut cards: Vec<CardSpec> = Vec::new();
                    match ch.kind {
                        Kind::Gpu => {
                            if let Some(g) = gpu {
                                if let Some(aux) = self.gpu_aux.get(&g.id) {
                                    cards.push(CardSpec {
                                        key: format!("{}:vram", g.id),
                                        title: "VRAM",
                                        subtitle: if g.vram_total_mb >= 1024.0 {
                                            format!("of {}", util::fmt_mb(g.vram_total_mb))
                                        } else {
                                            "shared with ram".to_string()
                                        },
                                        dq: aux.vram.candles(self.tf),
                                        dropped: aux.vram.dropped(self.tf),
                                        unit: "GB",
                                        decimals: 2,
                                        min_span: 0.02,
                                    });
                                    if g.temp_c.is_some() {
                                        cards.push(CardSpec {
                                            key: format!("{}:temp", g.id),
                                            title: "TEMP",
                                            subtitle: "gpu core".to_string(),
                                            dq: aux.temp.candles(self.tf),
                                            dropped: aux.temp.dropped(self.tf),
                                            unit: "\u{00b0}C",
                                            decimals: 0,
                                            min_span: 2.0,
                                        });
                                    }
                                    if g.power_w.is_some() {
                                        cards.push(CardSpec {
                                            key: format!("{}:power", g.id),
                                            title: "POWER",
                                            subtitle: "board".to_string(),
                                            dq: aux.power.candles(self.tf),
                                            dropped: aux.power.dropped(self.tf),
                                            unit: "W",
                                            decimals: 1,
                                            min_span: 4.0,
                                        });
                                    }
                                }
                            }
                        }
                        Kind::Mem => {
                            for (id, title, span, empty_note) in [
                                (CH_COMMIT, "COMMIT", 0.5, "commit unavailable"),
                                (CH_SWAP, "SWAP", 0.05, "no page file"),
                            ] {
                                let Some(c) = self.channels.iter().find(|c| c.id == id) else {
                                    continue;
                                };
                                let Some(s) = self.series.get(id) else {
                                    continue;
                                };
                                let total = c.total.unwrap_or(0.0);
                                cards.push(CardSpec {
                                    key: id.to_string(),
                                    title,
                                    subtitle: if total > 0.0 {
                                        format!(
                                            "of {:.1} GB \u{00b7} {:.0}%",
                                            total,
                                            c.value / total * 100.0
                                        )
                                    } else {
                                        empty_note.to_string()
                                    },
                                    dq: s.candles(self.tf),
                                    dropped: s.dropped(self.tf),
                                    unit: "GB",
                                    decimals: 2,
                                    min_span: span,
                                });
                            }
                        }
                        _ => {}
                    }
                    draw_cards(ui, rect, &cards, &mut card_views, self.tf);
                }

                // ---- draggable edges --------------------------------------
                // These are registered after the chart, so they win the hit
                // test against its pan gesture on the few pixels they share.
                let screen = ui.ctx().screen_rect();
                let left_max = (screen.width() * 0.32).clamp(180.0, 340.0);
                let right_max = (screen.width() * 0.34).clamp(180.0, 360.0);

                let lrect = Rect::from_min_max(
                    pos2(full.left() - 3.0, full.top()),
                    pos2(full.left() + 3.0, full.bottom()),
                );
                let lr = ui.interact(lrect, egui::Id::new("fx_watch_edge"), Sense::drag());
                if lr.hovered() || lr.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }
                if lr.dragged() {
                    if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
                        self.watch_w = snap_width(p.x, left_max);
                    }
                }
                if self.watch_w < 1.0 {
                    // Collapsed panels leave a grab tab behind: an edge with
                    // nothing on it is impossible to find again.
                    let col = if lr.hovered() { th::MA20 } else { th::DIM };
                    let tab = Rect::from_min_size(
                        pos2(full.left(), full.center().y - 22.0),
                        vec2(6.0, 44.0),
                    );
                    ui.painter().rect_filled(tab, 2.0, col);
                }

                let rrect = Rect::from_min_max(
                    pos2(full.right() - 3.0, full.top()),
                    pos2(full.right() + 3.0, full.bottom()),
                );
                let rr = ui.interact(rrect, egui::Id::new("fx_procs_edge"), Sense::drag());
                if rr.hovered() || rr.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }
                if rr.dragged() {
                    if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
                        self.procs_w = snap_width(screen.right() - p.x, right_max);
                    }
                }
                if self.procs_w < 1.0 {
                    let col = if rr.hovered() { th::MA20 } else { th::DIM };
                    let tab = Rect::from_min_size(
                        pos2(full.right() - 6.0, full.center().y - 22.0),
                        vec2(6.0, 44.0),
                    );
                    ui.painter().rect_filled(tab, 2.0, col);
                }

                // ---- sensor strip divider ---------------------------------
                if can_strip {
                    if with_strip {
                        let edge_y = full.bottom() - strip_h;
                        let drect = Rect::from_min_max(
                            pos2(full.left(), edge_y - 3.0),
                            pos2(full.right(), edge_y + 3.0),
                        );
                        let dr =
                            ui.interact(drect, egui::Id::new("fx_strip_edge"), Sense::drag());
                        if dr.hovered() || dr.dragged() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                        }
                        if dr.dragged() {
                            if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
                                self.strip_h = snap_strip(full.bottom() - p.y);
                            }
                        }
                        let col = if dr.hovered() || dr.dragged() {
                            th::MA20
                        } else {
                            th::LINE
                        };
                        ui.painter().hline(
                            (full.left() + 1.0)..=(full.right() - 1.0),
                            edge_y,
                            egui::Stroke::new(1.0_f32, col),
                        );
                    } else {
                        // Collapsed: a tab wide enough to find without hunting.
                        let tab = Rect::from_center_size(
                            pos2(full.center().x, full.bottom() - 6.0),
                            vec2(64.0, 6.0),
                        );
                        let tr = ui.interact(tab, egui::Id::new("fx_strip_tab"), Sense::drag());
                        if tr.hovered() || tr.dragged() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                        }
                        if tr.dragged() {
                            if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
                                self.strip_h = snap_strip(full.bottom() - p.y);
                            }
                        }
                        let col = if tr.hovered() || tr.dragged() {
                            th::MA20
                        } else {
                            th::DIM
                        };
                        ui.painter().rect_filled(tab, 3.0, col);
                    }
                }
            });

        self.card_views = card_views;
    }
}

impl eframe::App for FxApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Fit the opening window to the monitor before anything is laid out:
        // the design size assumes a roomy screen, and a window that does not
        // fit ends up with its bottom row behind the taskbar.
        self.fit_to_work_area(ctx);

        // winit re-adds WS_CAPTION | WS_BORDER after flag changes; where the
        // compositor honours that frame, the whole UI is drawn offset by it.
        self.frame_fix.tick();

        // Tray thread: readiness first, then clicks from the icon.
        while let Ok(msg) = self.tray_rx.try_recv() {
            match msg {
                crate::tray::TrayMsg::Ready(tray) => {
                    self.tray_ready = true;
                    self.tray_hwnd = tray;
                }
                crate::tray::TrayMsg::ToggleFullscreen => {
                    self.fullscreen = !self.fullscreen;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
                    self.sync_tray();
                }
                crate::tray::TrayMsg::Quit => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        // F11 toggles fullscreen.
        if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
            self.sync_tray();
        }

        // Invisible resize borders along the window edges. The OS frame is
        // gone, so the native modal resize loop is driven from here instead:
        // each zone fires `BeginResize` on drag, which winit maps to a
        // `WM_NCLBUTTONDOWN` with the matching hit-test code — including the
        // restore-then-resize dance on a maximized window.
        if !self.fullscreen {
            let screen = ctx.screen_rect();
            const E: f32 = 5.0;
            let zones = [
                (egui::ResizeDirection::North, Rect::from_min_max(screen.left_top(), pos2(screen.right(), screen.top() + E))),
                (egui::ResizeDirection::South, Rect::from_min_max(pos2(screen.left(), screen.bottom() - E), screen.right_bottom())),
                (egui::ResizeDirection::West, Rect::from_min_max(screen.left_top(), pos2(screen.left() + E, screen.bottom()))),
                (egui::ResizeDirection::East, Rect::from_min_max(pos2(screen.right() - E, screen.top()), screen.right_bottom())),
                (egui::ResizeDirection::NorthWest, Rect::from_min_max(screen.left_top(), pos2(screen.left() + E, screen.top() + E))),
                (egui::ResizeDirection::NorthEast, Rect::from_min_max(pos2(screen.right() - E, screen.top()), pos2(screen.right(), screen.top() + E))),
                (egui::ResizeDirection::SouthWest, Rect::from_min_max(pos2(screen.left(), screen.bottom() - E), pos2(screen.left() + E, screen.bottom()))),
                (egui::ResizeDirection::SouthEast, Rect::from_min_max(pos2(screen.right() - E, screen.bottom() - E), screen.right_bottom())),
            ];
            for (i, (dir, zrect)) in zones.iter().enumerate() {
                egui::Area::new(egui::Id::new(("fx_rz", i)))
                    .fixed_pos(zrect.min)
                    .order(egui::Order::Foreground)
                    .show(ctx, |ui| {
                        let (_, resp) = ui.allocate_exact_size(zrect.size(), egui::Sense::drag());
                        if resp.drag_started_by(egui::PointerButton::Primary) {
                            if self.frame_fix.manual_resize() {
                                self.frame_fix.begin_resize(*dir, ctx.pixels_per_point());
                            } else {
                                ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(*dir));
                            }
                        }
                        if resp.drag_stopped_by(egui::PointerButton::Primary) {
                            self.frame_fix.end_resize();
                        }
                        // Only while the pointer is actually in *this* zone:
                        // an unconditional set would let the last zone's
                        // diagonal cursor win across the whole window.
                        if resp.hovered() || resp.dragged() {
                            ctx.set_cursor_icon(match dir {
                                egui::ResizeDirection::North | egui::ResizeDirection::South => egui::CursorIcon::ResizeVertical,
                                egui::ResizeDirection::East | egui::ResizeDirection::West => egui::CursorIcon::ResizeHorizontal,
                                egui::ResizeDirection::NorthWest | egui::ResizeDirection::SouthEast => egui::CursorIcon::ResizeNorthWest,
                                egui::ResizeDirection::NorthEast | egui::ResizeDirection::SouthWest => egui::CursorIcon::ResizeNorthEast,
                            });
                        }
                    });
            }

            // A hairline outline replaces the frame the OS used to draw:
            // without it the window melts into a dark desktop.
            if !ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
                ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("fx_frame")))
                    .rect_stroke(screen.shrink(0.5), 0.0, egui::Stroke::new(1.0_f32, th::LINE));
            }
        }

        while let Ok(snap) = self.sampler.rx.try_recv() {
            // CORE charts the pinned core when one is selected, otherwise the
            // busiest core of this sample.
            let core_val = match self.core_pick {
                Some(i) => snap.cores.get(i).copied().unwrap_or_else(|| {
                    snap.channels
                        .iter()
                        .find(|c| c.kind == Kind::Core)
                        .map(|c| c.value)
                        .unwrap_or(0.0)
                }),
                None => snap
                    .channels
                    .iter()
                    .find(|c| c.kind == Kind::Core)
                    .map(|c| c.value)
                    .unwrap_or(0.0),
            };

            // One candle stream per instrument; the flow under it is whatever
            // explains that instrument's moves.
            for ch in &snap.channels {
                let v = if ch.kind == Kind::Core {
                    core_val
                } else {
                    ch.value
                };
                self.series
                    .entry(ch.id.clone())
                    .or_default()
                    .push(snap.wall, v, ch.flow);
            }
            let live: HashSet<&str> = snap.channels.iter().map(|c| c.id.as_str()).collect();
            self.series.retain(|k, _| live.contains(k.as_str()));

            self.channels = snap.channels.clone();
            self.gpus = snap.gpus.clone();
            // Guarded: a tick where the driver enumerates no adapter must not
            // wipe the strip's history — that is what made the panes come back
            // empty after leaving the page.
            if !snap.gpus.is_empty() {
                self.gpu_aux
                    .retain(|id, _| snap.gpus.iter().any(|g| g.id == *id));
            }

            if !self.channels.iter().any(|c| c.id == self.selected) {
                let id = CH_CPU.to_string();
                self.selected = String::new();
                self.select(&id);
            }

            // GPU sensor history only advances while the GPU page is open: the
            // sampler refreshes those counters at 4 Hz here and once every 15 s
            // elsewhere, and mixing the two cadences would draw a staircase.
            if self.selected_kind() == Some(Kind::Gpu) {
                for g in &self.gpus {
                    let aux = self.gpu_aux.entry(g.id.clone()).or_default();
                    // The first PDH collection returns nothing, and a leading
                    // zero would set the card's scale to 0..total for minutes.
                    // Nothing is pushed until the sensor has produced a real
                    // reading, or until the series already has one.
                    // Stored in GB: the card and its axis must quote one unit,
                    // and a 1022 MB reading under a 1.00 GB axis is a bug that
                    // only shows up on machines with round numbers.
                    if g.vram_used_mb > 0.0 || aux.vram.last(0).is_some() {
                        aux.vram.push(snap.wall, g.vram_used_mb / 1024.0, 0.0);
                    }
                    if let Some(t) = g.temp_c {
                        aux.temp.push(snap.wall, t, 0.0);
                    }
                    if let Some(w) = g.power_w {
                        aux.power.push(snap.wall, w, 0.0);
                    }
                }
            }

            self.remember_cores(&snap);
            // Only advance the per-position history when the process table was
            // actually re-enumerated; the snapshot repeats it four times.
            if snap.procs_gen != self.procs_gen {
                self.procs_gen = snap.procs_gen;
                let now = Instant::now();
                for row in &snap.procs {
                    let pos = self.mini.entry(row.pid).or_insert_with(|| MiniPos {
                        seen: now,
                        q: VecDeque::with_capacity(MINI_SAMPLES),
                        qm: VecDeque::with_capacity(MINI_SAMPLES),
                        qg: VecDeque::with_capacity(MINI_SAMPLES),
                    });
                    pos.seen = now;
                    if self.metric == METRIC_MEM {
                        pos.qm.push_back(row.mem_mb);
                        while pos.qm.len() > MINI_SAMPLES {
                            pos.qm.pop_front();
                        }
                    } else if self.metric == METRIC_GPU {
                        pos.qg.push_back(row.gpu_pct);
                        while pos.qg.len() > MINI_SAMPLES {
                            pos.qg.pop_front();
                        }
                    } else {
                        pos.q.push_back(row.cpu);
                        while pos.q.len() > MINI_SAMPLES {
                            pos.q.pop_front();
                        }
                    }
                }
                self.mini
                    .retain(|_, pos| now.duration_since(pos.seen).as_secs() < MINI_TTL_SECS);
            }
            self.last = Some(snap);
        }
        while let Ok(fill) = self.sampler.fills.try_recv() {
            self.fill = Some((fill, Instant::now()));
        }

        // Holding a position needs a smooth progress sweep, so it repaints every
        // frame; otherwise the sampler's 4 Hz is all this window ever needs.
        if self.hold.is_some() {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }

        self.header(ctx);
        self.status_bar(ctx);
        self.watchlist(ctx);
        self.processes(ctx);

        self.chart(ctx);

        if self
            .fill
            .as_ref()
            .map_or(false, |(_, at)| at.elapsed().as_secs_f32() > 8.0)
        {
            self.fill = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn stale(pid: u32) -> Option<(u32, Instant)> {
        Some((pid, Instant::now() - Duration::from_secs(2)))
    }

    #[test]
    fn hold_fires_once_after_the_threshold() {
        let mut hold = stale(7);
        let mut locked = false;
        assert_eq!(hold_progress(&mut hold, &mut locked, 7, true), 1.0);
        assert!(hold.is_none(), "press record must be cleared");
        assert!(locked, "must lock out the still-held button");
        assert_eq!(hold_progress(&mut hold, &mut locked, 7, true), 0.0);
    }

    #[test]
    fn release_clears_the_press() {
        let mut hold = Some((7, Instant::now()));
        let mut locked = true;
        assert_eq!(hold_progress(&mut hold, &mut locked, 7, false), 0.0);
        // The frame-level pointer check also resets `locked`; the row-level call
        // must not fire while the button is up.
        assert_eq!(hold_progress(&mut hold, &mut locked, 7, true), 0.0);
        assert!(hold.is_some());
    }

    #[test]
    fn sliding_onto_another_row_restarts_the_timer() {
        let mut hold = stale(7);
        let mut locked = false;
        assert_eq!(hold_progress(&mut hold, &mut locked, 9, true), 0.0);
        match hold {
            Some((pid, _)) => assert_eq!(pid, 9),
            None => panic!("hold should have moved to the new row"),
        }
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    #[test]
    fn refuses_positions_that_take_the_desktop_with_them() {
        assert_eq!(blocked("explorer.exe", 3816), Some("system shell"));
        assert_eq!(blocked("DWM.exe", 1200), Some("system shell"));
        assert_eq!(blocked("lsass.exe", 900), Some("system critical"));
        assert_eq!(blocked("csrss.exe", 700), Some("system critical"));
        assert_eq!(blocked("anything", 4), Some("system position"));
        assert_eq!(blocked("anything", std::process::id()), Some("own position"));
        assert_eq!(blocked("chrome.exe", 8000), None);
    }

    #[test]
    fn core_grid_fits_every_thread_on_a_workstation() {
        // 8 threads in a normal panel: one row of eight.
        let g = core_grid(8, 186.0, 700.0);
        assert_eq!((g.cols, g.visible), (8, 8));
        assert_eq!(g.h, 13.0);

        // 32 threads at quarter-screen panel width: eight columns, four even
        // rows, every thread shown — the old code capped at 24.
        let g = core_grid(32, 156.0, 362.0);
        assert_eq!(g.cols, 8);
        assert_eq!(g.visible, 32);
        assert_eq!(g.h, 13.0);

        // Impossible counts still produce a multiple of four columns.
        for n in [6usize, 12, 20, 40] {
            let g = core_grid(n, 156.0, 700.0);
            assert_eq!(g.cols % 4, 0, "n={n}");
            assert!(g.visible <= n);
        }

        // Every chip, plus the gaps, has to stay inside the panel.
        for n in [4usize, 8, 16, 32, 48] {
            for w in [120.0f32, 156.0, 186.0, 332.0] {
                let g = core_grid(n, w, 700.0);
                let used = g.cols as f32 * g.w + CORE_GAP * (g.cols as f32 - 1.0);
                assert!(used <= w + 0.01, "n={n} w={w} used={used}");
                assert!(g.visible <= n);
            }
        }
    }

    #[test]
    fn dragging_a_panel_away_ends_at_zero() {
        assert_eq!(snap_width(0.4, 300.0), 0.0);
        assert_eq!(snap_width(47.0, 300.0), 0.0);
        assert_eq!(snap_width(60.0, 300.0), 60.0);
        assert_eq!(snap_width(900.0, 300.0), 300.0);
    }

    #[test]
    fn dragging_the_strip_away_ends_at_zero() {
        assert_eq!(snap_strip(10.0), 0.0);
        assert_eq!(snap_strip(60.0), 60.0);
        assert_eq!(snap_strip(999.0), STRIP_MAX);
    }

    #[test]
    fn core_grid_shrinks_cells_before_dropping_cores() {
        // 256 threads cannot all fit in a 362 pt panel; cells go to their
        // minimum height and the remainder is reported rather than hidden.
        let g = core_grid(256, 156.0, 362.0);
        assert!(g.h >= CORE_MIN_H);
        assert!(g.visible < 256);
        // It should still be showing the overwhelming majority.
        assert!(g.visible >= 90, "visible = {}", g.visible);
    }
}
