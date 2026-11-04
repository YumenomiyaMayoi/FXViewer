//! Candlestick chart, drawn straight onto the egui painter.
//!
//! No plotting crate involved: a candle is a 1px wick and a filled body, so a
//! few hundred of them cost almost nothing per frame.

use std::collections::VecDeque;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Ui};

use crate::series::{Candle, Series};
use crate::theme as th;
use crate::util::{fmt_num, time_label};

/// Direction colour for a candle: red rising, green falling, neutral grey
/// when it closed exactly where it opened. Flat buckets are not rare —
/// Windows' GPU counters only update once per second, so every 1 s GPU
/// candle is a doji — and the old `c >= o` test painted all of them red,
/// which made an idle card look like a wall of buying.
fn candle_color(c: &Candle) -> Color32 {
    if c.c > c.o {
        th::UP
    } else if c.c < c.o {
        th::DOWN
    } else {
        th::FLAT
    }
}

/// One wheel step. Zoom-in acts on the *visible* window: a young buffer
/// leaves the stored preference far above what can be drawn, and acting on
/// the preference would spend dozens of notches in the invisible range, so
/// the wheel feels stuck until it catches down. Zoom-out acts on the
/// preference, so scrolling out towards "everything" accumulates even while
/// the buffer is still short.
fn zoom_step(view_shown: f32, visible: f32, scroll: f32, cap: f32) -> f32 {
    let factor = (1.0 - scroll * 0.0022).clamp(0.86, 1.16);
    let base = if factor < 1.0 { visible } else { view_shown };
    (base * factor).clamp(8.0, cap)
}

pub struct ChartOpts {
    pub title: String,
    pub subtitle: String,
    pub decimals: usize,
    /// Unit printed next to the headline value and above the axis: `"%"`,
    /// `"GB"`, or empty for a rate whose unit the subtitle already names.
    ///
    /// It also decides the axis treatment: percentages are clamped to 0..100
    /// because that is the whole scale, while memory is plotted in GB and has
    /// to use the range it actually occupies.
    pub unit: &'static str,
    /// Smallest span the value axis will show, in the instrument's own unit.
    /// Keeps an idle series from zooming into jitter.
    pub min_span: f32,
    pub tf: usize,
    /// What the volume bars are measuring. They change with the instrument, so
    /// the label has to come from the caller rather than being hard coded.
    pub vol_label: String,
    /// Throughput instruments measure volume directly, so a second pane of the
    /// same series would just be the candles drawn twice. Those pages drop the
    /// pane and give the chart the height instead.
    pub show_volume: bool,
    /// A sensor pane under a chart. Same renderer and the same gestures as the
    /// main chart — wheel to zoom, drag to pan, hover for a crosshair — with
    /// the legend reduced to one line so a 160 px pane still has room to draw.
    pub compact: bool,
}

pub struct ChartView {
    /// How many candles the user wants to see.
    pub shown: f32,
    /// How many candles are scrolled away from the right edge.
    pub offset: f32,
    /// State of the previous frame, used to keep a scrolled-back window
    /// anchored to the same candles while the ring buffer rolls over.
    last_n: usize,
    last_dropped: u64,
}

impl ChartView {
    pub fn new() -> Self {
        Self {
            shown: 150.0,
            offset: 0.0,
            last_n: 0,
            last_dropped: 0,
        }
    }
    /// Snap back to the newest candle.
    pub fn follow(&mut self) {
        self.offset = 0.0;
        self.last_n = 0;
        self.last_dropped = 0;
    }

    /// Shift the scroll offset by however many candles entered or left the
    /// buffer, so history does not slide forward under the cursor.
    pub fn anchor(&mut self, n: usize, dropped: u64) {
        if self.last_n == 0 {
            self.last_n = n;
            self.last_dropped = dropped;
            return;
        }
        if self.offset > 0.0 {
            let grew = n.saturating_sub(self.last_n);
            let evicted = dropped.saturating_sub(self.last_dropped) as usize;
            let shift = grew + evicted;
            if shift > 0 {
                self.offset += shift as f32;
            }
        }
        self.last_n = n;
        self.last_dropped = dropped;
    }
}

#[derive(Default)]
pub struct ChartOut {
    pub hovered: Option<usize>,
}

/// Draws the main chart inside `rect`. The caller reserves the space, so a
/// sensor strip can live underneath without this function having to know about
/// it.
pub fn show(
    ui: &mut Ui,
    rect: Rect,
    view: &mut ChartView,
    dq: &VecDeque<Candle>,
    dropped: u64,
    opts: &ChartOpts,
) -> ChartOut {
    let resp = ui.allocate_rect(rect, Sense::click_and_drag());
    let rect = resp.rect;
    let compact = opts.compact;
    let p = ui.painter_at(rect);
    p.rect_filled(rect, if compact { 3.0 } else { 0.0 }, th::BG);
    if compact {
        // A hairline is what separates one sensor pane from the next; the main
        // chart is already bounded by the panel itself.
        p.rect_stroke(
            rect.shrink(0.5),
            3.0,
            Stroke::new(1.0_f32, th::LINE),
        );
    }

    // A chart dropped into a quarter tile is only ~390x360 points. At the
    // normal proportions the legend (80) and the volume pane (~62) between them
    // take 40 % of that, which is why the candles looked squeezed. In this mode
    // the legend tightens and the volume pane steps aside; the shape of the
    // price is what the tile is for.
    let tight = !compact && rect.width() < 620.0 && rect.height() < 470.0;
    let axis_w = if compact {
        46.0
    } else if tight {
        56.0
    } else {
        66.0
    };
    let legend_h = if compact {
        36.0
    } else if tight {
        60.0
    } else {
        80.0
    };
    let time_h = if compact { 14.0 } else { 20.0 };
    let show_vol = opts.show_volume && !tight;
    let vol_h = if show_vol {
        (rect.height() * 0.17).clamp(44.0, 110.0)
    } else {
        0.0
    };

    let plot_left = rect.left() + 8.0;
    let plot_right = rect.right() - axis_w;
    let plot_top = rect.top() + legend_h;
    let vol_bottom = rect.bottom() - time_h;
    let plot_bottom = if show_vol {
        vol_bottom - vol_h - 8.0
    } else {
        vol_bottom - 6.0
    };

    let min_w = if compact { 60.0 } else { 90.0 };
    let min_h = if compact { 34.0 } else { 60.0 };
    if plot_right - plot_left < min_w || plot_bottom - plot_top < min_h {
        p.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "window too small",
            FontId::proportional(if compact { 9.5 } else { 12.0 }),
            th::MUTED,
        );
        return ChartOut::default();
    }

    let plot = Rect::from_min_max(
        egui::pos2(plot_left, plot_top),
        egui::pos2(plot_right, plot_bottom),
    );
    let vrect = if show_vol {
        Rect::from_min_max(
            egui::pos2(plot_left, plot_bottom + 8.0),
            egui::pos2(plot_right, vol_bottom),
        )
    } else {
        Rect::NOTHING
    };

    p.vline(plot_right, plot.y_range(), Stroke::new(1.0_f32, th::LINE));
    p.hline(plot.x_range(), plot_bottom, Stroke::new(1.0_f32, th::LINE));

    let n = dq.len();
    if n < 2 {
        draw_legend(&p, rect, dq, opts, None, tight);
        p.text(
            plot.center(),
            Align2::CENTER_CENTER,
            "collecting first candles…",
            FontId::proportional(if compact { 9.5 } else { 12.0 }),
            th::MUTED,
        );
        return ChartOut::default();
    }

    let n_f = n as f32;
    let hover_pos = resp.hover_pos();

    // ---- navigation -------------------------------------------------------
    // `view.shown` is the user's zoom preference and is never shrunk just
    // because the buffer is still filling up; `shown` is what gets drawn.
    // `max_window` never drops below the minimum zoom, otherwise `clamp` panics
    // during the first seconds of a session.
    // Zoom-out is capped by pixel density: below about two pixels per candle
    // the wicks merge into a solid block. Looking further back is what the
    // coarser timeframes are for.
    let density_cap = (plot.width() / 1.8).clamp(8.0, 1500.0);
    // The main chart stretches a young buffer to fill the pane, which is right
    // for a chart you sit and watch. A sensor pane is read at a glance: it
    // keeps its pitch and fills in from the right, so forty samples look like
    // forty candles instead of forty fat blocks.
    let max_window = if compact {
        density_cap
    } else {
        density_cap.min(n_f.max(8.0))
    };
    let mut shown = view.shown.clamp(8.0, max_window);
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll.abs() > 0.1 {
            view.shown = zoom_step(view.shown, shown, scroll, density_cap);
            shown = view.shown.clamp(8.0, max_window);
        }
    }

    view.anchor(n, dropped);

    let max_off = (n_f - shown).max(0.0);
    if resp.dragged() {
        let px_per = plot.width() / shown.max(1.0);
        if px_per > 0.0 {
            view.offset += resp.drag_delta().x / px_per;
        }
    }
    view.offset = view.offset.clamp(0.0, max_off);

    let end = n_f - view.offset;
    let start = end - shown;
    let i0 = start.floor().max(0.0) as usize;
    let i1 = (end.ceil() as usize).min(n).max(i0 + 1);

    // ---- value range ------------------------------------------------------
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for i in i0..i1 {
        if let Some(c) = dq.get(i) {
            if c.l < lo {
                lo = c.l;
            }
            if c.h > hi {
                hi = c.h;
            }
        }
    }
    if !lo.is_finite() || !hi.is_finite() {
        lo = 0.0;
        hi = 1.0;
    }
    let pad = ((hi - lo) * 0.12).max(1e-4);
    lo -= pad;
    hi += pad;

    // A flat series would otherwise collapse the axis into a sliver where every
    // tick label rounds to the same number.
    let pct = opts.unit == "%";
    let min_span = if pct {
        4.0
    } else if opts.min_span > 0.0 {
        // The caller knows the instrument: 20 MB is a readable slice of VRAM,
        // 0.5 GB of system memory, 2 GB of swap. A relative floor would scale
        // with the reading and leave a nearly flat series pinned to the top of
        // the pane.
        opts.min_span
    } else {
        (hi.abs().max(lo.abs()) * 0.05).max(1e-3)
    };
    if hi - lo < min_span {
        let mid = (lo + hi) * 0.5;
        lo = mid - min_span * 0.5;
        hi = mid + min_span * 0.5;
    }
    if pct {
        if lo < 0.0 {
            hi -= lo;
            lo = 0.0;
        }
        if hi > 100.0 {
            lo -= hi - 100.0;
            hi = 100.0;
        }
        lo = lo.max(0.0);
        hi = hi.min(100.0);
    } else if lo < 0.0 && hi > 0.0 {
        // Every series this program draws is a magnitude. The padding below
        // zero shows up as a "-0.00" tick, which reads like a broken reading.
        // Keeping the span matters: an idle series that just had its floor
        // raised to `min_span` would otherwise be cut in half by this clamp and
        // zoom straight back into jitter.
        let span = hi - lo;
        lo = 0.0;
        hi = hi.max(span);
    }
    let vrange = (hi - lo).max(1e-6);
    let hi = lo + vrange;

    let px_per = plot.width() / shown;
    let x_of = |i: f32| plot.left() + (i - start) * px_per;
    let y_of = |v: f32| plot.bottom() - (v - lo) / vrange * plot.height();

    // ---- horizontal grid & value axis ------------------------------------
    if !opts.unit.is_empty() {
        // On a card there is no room above the axis: the legend already has two
        // lines in it, so the unit moves to the top-right corner.
        let (pos, align, font) = if compact {
            (
                egui::pos2(rect.right() - 7.0, rect.top() + 4.0),
                Align2::RIGHT_TOP,
                8.5,
            )
        } else {
            (
                egui::pos2(plot_right + 8.0, plot_top - 6.0),
                Align2::LEFT_BOTTOM,
                9.5,
            )
        };
        p.text(pos, align, opts.unit, FontId::proportional(font), th::DIM);
    }
    let step_v = nice_step(vrange, if compact { 3.0 } else { 5.0 });
    if step_v > 0.0 {
        // Precision follows the tick step, not the data range, otherwise a
        // narrow axis prints the same rounded number on every line. Derived
        // from the step's magnitude so 0.005 gets three decimals instead of
        // two labels that both read "1.00".
        let decimals = tick_decimals(step_v);
        let mut v = (lo / step_v).ceil() * step_v;
        let mut guard = 0;
        while v <= hi && guard < 40 {
            let y = y_of(v);
            p.hline(plot.x_range(), y, Stroke::new(1.0_f32, th::GRID));
            p.text(
                egui::pos2(plot_right + 8.0, y),
                Align2::LEFT_CENTER,
                fmt_num(v, decimals).as_str(),
                FontId::monospace(if compact { 8.5 } else { 10.5 }),
                th::DIM,
            );
            v += step_v;
            guard += 1;
        }
    }

    // ---- vertical grid & time axis ---------------------------------------
    // Space the gridlines over the candles that exist, not over the requested
    // window: a young buffer only occupies the right edge, and dividing `shown`
    // would leave a single gridline stranded next to it.
    let visible = i1.saturating_sub(i0).max(1) as f32;
    // A stamp is eight characters wide. On a quarter-screen tile the pane is
    // only a few hundred pixels across, and a fixed count of five labels put
    // them on top of each other; the spacing follows the pixel budget instead.
    let min_gap = if compact { 54.0 } else { 64.0 };
    let by_width = (min_gap / px_per.max(0.5)).ceil() as usize;
    let k = ((visible / if compact { 3.0 } else { 5.0 }).round() as usize)
        .max(by_width)
        .max(1);
    // The crosshair paints its own timestamp over the axis. A label sitting
    // underneath it would show through as a stray fragment, so the one it
    // covers is dropped instead of overdrawn.
    let tag_w = if compact { 54.0 } else { 70.0 };
    let tag_span = hover_pos.filter(|hp| plot.contains(*hp)).map(|hp| {
        let cx = hp.x.clamp(plot_left + tag_w * 0.5, plot_right - tag_w * 0.5);
        (cx - tag_w * 0.5 - 4.0, cx + tag_w * 0.5 + 4.0)
    });

    let mut i = ((i0 as f32 / k as f32).ceil() as usize) * k;
    while i < i1 {
        if let Some(c) = dq.get(i) {
            let x = x_of(i as f32 + 0.5);
            let covered = tag_span.map_or(false, |(a, b)| x > a && x < b);
            if !covered && x > plot_left + 26.0 && x < plot_right - 26.0 {
                p.vline(x, plot.y_range(), Stroke::new(1.0_f32, th::GRID));
                p.text(
                    egui::pos2(x, vol_bottom + 3.0),
                    Align2::CENTER_TOP,
                    time_label(c.t, opts.tf).as_str(),
                    FontId::monospace(if compact { 8.5 } else { 10.0 }),
                    th::DIM,
                );
            }
        }
        i += k;
    }

    // ---- candles ----------------------------------------------------------
    let body_w = (px_per * 0.62).clamp(1.0, 24.0);
    for i in i0..i1 {
        let c = match dq.get(i) {
            Some(c) => *c,
            None => continue,
        };
        let x = x_of(i as f32 + 0.5);
        if x < plot_left - 8.0 || x > plot_right + 8.0 {
            continue;
        }
        let col = candle_color(&c);
        // The wick is exactly high-to-low and the body exactly open-to-close.
        // Drawing a guaranteed stub above and below made quiet instruments look
        // busier than they are, which is the one thing a monitor must never do.
        let yo = y_of(c.o);
        let yc = y_of(c.c);
        let bh = (yo - yc).abs().max(1.0);
        // A doji still needs a body one pixel tall; centring it on the open
        // keeps that pixel where the price actually is.
        let top = (yo + yc) * 0.5 - bh * 0.5;
        p.vline(
            x,
            egui::Rangef::new(y_of(c.h), y_of(c.l)),
            Stroke::new(1.0_f32, col),
        );
        p.rect_filled(
            Rect::from_min_size(egui::pos2(x - body_w * 0.5, top), egui::vec2(body_w, bh)),
            0.0,
            col,
        );
    }

    // ---- moving averages --------------------------------------------------
    let xr = &x_of;
    let yr = &y_of;
    draw_ma(&p, dq, i0, i1, 5, xr, yr, th::MA5);
    draw_ma(&p, dq, i0, i1, 20, xr, yr, th::MA20);

    // ---- volume -----------------------------------------------------------
    if show_vol {
        let mut vmax = 0.0f32;
        for i in i0..i1 {
            if let Some(c) = dq.get(i) {
                if c.v > vmax {
                    vmax = c.v;
                }
            }
        }
        let vmax = vmax.max(1e-4);
        for i in i0..i1 {
            let c = match dq.get(i) {
                Some(c) => *c,
                None => continue,
            };
            let x = x_of(i as f32 + 0.5);
            if x < plot_left - 8.0 || x > plot_right + 8.0 {
                continue;
            }
            let h = (c.v / vmax) * vrect.height();
            let col = candle_color(&c);
            p.rect_filled(
                Rect::from_min_max(
                    egui::pos2(x - body_w * 0.5, vrect.bottom() - h),
                    egui::pos2(x + body_w * 0.5, vrect.bottom()),
                ),
                0.0,
                col.gamma_multiply(0.5),
            );
        }
        let vol_label_pos = egui::pos2(vrect.left() + 2.0, vrect.top() + 1.0);
        p.rect_filled(
            Rect::from_min_size(vol_label_pos, egui::vec2(140.0, 13.0)),
            2.0,
            th::BG,
        );
        p.text(
            egui::pos2(vol_label_pos.x + 3.0, vol_label_pos.y + 1.0),
            Align2::LEFT_TOP,
            opts.vol_label.as_str(),
            FontId::proportional(9.5),
            th::DIM,
        );
    }

    // ---- last price -------------------------------------------------------
    if let Some(last) = dq.back() {
        let col = candle_color(last);
        let y = y_of(last.c);
        if y >= plot.top() && y <= plot.bottom() {
            dashed_hline(&p, plot_left, plot_right, y, Stroke::new(1.0_f32, col.gamma_multiply(0.65)));
            let th_half = if compact { 6.5 } else { 8.0 };
            let tag = Rect::from_min_max(
                egui::pos2(plot_right + 2.0, y - th_half),
                egui::pos2(rect.right() - 2.0, y + th_half),
            );
            p.rect_filled(tag, 2.0, col);
            p.text(
                tag.center(),
                Align2::CENTER_CENTER,
                fmt_num(last.c, opts.decimals).as_str(),
                FontId::monospace(if compact { 8.5 } else { 10.5 }),
                Color32::WHITE,
            );
        }
    }

    // ---- crosshair --------------------------------------------------------
    let mut out = ChartOut::default();
    if let Some(hp) = hover_pos {
        if plot.contains(hp) {
            let muted = th::MUTED.gamma_multiply(0.55);
            p.vline(hp.x, plot.y_range(), Stroke::new(1.0_f32, muted));
            p.hline(plot.x_range(), hp.y, Stroke::new(1.0_f32, muted));

            let v = lo + (plot.bottom() - hp.y) / plot.height() * vrange;
            let th_half = if compact { 6.5 } else { 8.0 };
            let tag = Rect::from_min_max(
                egui::pos2(plot_right + 2.0, hp.y - th_half),
                egui::pos2(rect.right() - 2.0, hp.y + th_half),
            );
            p.rect_filled(tag, 2.0, th::SURFACE);
            p.text(
                tag.center(),
                Align2::CENTER_CENTER,
                fmt_num(v, opts.decimals).as_str(),
                FontId::monospace(if compact { 8.5 } else { 10.5 }),
                th::TEXT2,
            );

            let idx_f = start + (hp.x - plot_left) / px_per;
            let idx = (idx_f.floor().max(0.0) as usize).min(n - 1);
            if let Some(c) = dq.get(idx) {
                let w = if compact { 54.0 } else { 70.0 };
                let cx = hp.x.clamp(plot_left + w * 0.5, plot_right - w * 0.5);
                let t = Rect::from_min_max(
                    egui::pos2(cx - w * 0.5, vol_bottom + 1.0),
                    egui::pos2(cx + w * 0.5, rect.bottom() - 1.0),
                );
                p.rect_filled(t, 2.0, th::SURFACE);
                p.text(
                    t.center(),
                    Align2::CENTER_CENTER,
                    time_label(c.t, opts.tf).as_str(),
                    FontId::monospace(if compact { 8.5 } else { 10.0 }),
                    th::TEXT2,
                );
                out.hovered = Some(idx);
            }
        }
    }

    draw_legend(&p, rect, dq, opts, out.hovered, tight);
    out
}

fn draw_legend(
    p: &egui::Painter,
    rect: Rect,
    dq: &VecDeque<Candle>,
    opts: &ChartOpts,
    hovered: Option<usize>,
    tight: bool,
) {
    let candle = hovered
        .and_then(|i| dq.get(i).copied())
        .or_else(|| dq.back().copied());

    if opts.compact {
        // Two lines are all a 160 px pane can afford: title and subtitle on the
        // first, value and delta on the second. The OHLC row and the MA labels
        // stay on the main chart, where there is room for them.
        let x = rect.left() + 9.0;
        let r = p.text(
            egui::pos2(x, rect.top() + 4.0),
            Align2::LEFT_TOP,
            opts.title.as_str(),
            FontId::proportional(9.0),
            th::MUTED,
        );
        if !opts.subtitle.is_empty() {
            p.text(
                egui::pos2(r.right() + 6.0, rect.top() + 4.5),
                Align2::LEFT_TOP,
                opts.subtitle.as_str(),
                FontId::proportional(8.5),
                th::DIM,
            );
        }
        let Some(c) = candle else { return };
        let big = p.text(
            egui::pos2(x, rect.top() + 15.0),
            Align2::LEFT_TOP,
            fmt_num(c.c, opts.decimals).as_str(),
            FontId::monospace(13.0),
            th::TEXT,
        );
        let mut nx = big.right() + 4.0;
        if !opts.unit.is_empty() {
            let u = p.text(
                egui::pos2(nx, rect.top() + 19.0),
                Align2::LEFT_TOP,
                opts.unit,
                FontId::proportional(8.5),
                th::MUTED,
            );
            nx = u.right() + 6.0;
        }
        let delta = c.c - c.o;
        let sign = if delta > 0.0 { "+" } else { "" };
        p.text(
            egui::pos2(nx, rect.top() + 19.0),
            Align2::LEFT_TOP,
            format!("{}{}", sign, fmt_num(delta, opts.decimals)).as_str(),
            FontId::monospace(8.5),
            candle_color(&c),
        );
        return;
    }

    // Two legend layouts. The tall one has room for a 25 pt quote and a full
    // OHLC row on separate lines; the tight one pulls them together so a
    // quarter-tile chart keeps its height for candles.
    let x = rect.left() + 12.0;
    let (y_title, y_chip, y_big, y_unit, y_delta, y_ohlc) = if tight {
        (6.0, 4.0, 17.0, 26.0, 28.0, 44.0)
    } else {
        (10.0, 8.0, 30.0, 42.0, 46.0, 62.0)
    };
    let r = p.text(
        egui::pos2(x, rect.top() + y_title),
        Align2::LEFT_TOP,
        opts.title.as_str(),
        FontId::proportional(if tight { 10.0 } else { 11.0 }),
        th::MUTED,
    );
    let mut tx = r.right() + 10.0;
    if !opts.subtitle.is_empty() {
        let r2 = p.text(
            egui::pos2(tx, rect.top() + y_title),
            Align2::LEFT_TOP,
            opts.subtitle.as_str(),
            FontId::proportional(if tight { 10.0 } else { 11.0 }),
            th::DIM,
        );
        tx = r2.right() + 10.0;
    }
    let chip = Rect::from_min_size(
        egui::pos2(tx, rect.top() + y_chip),
        egui::vec2(if tight { 26.0 } else { 28.0 }, if tight { 15.0 } else { 16.0 }),
    );
    p.rect_filled(chip, 3.0, th::SURFACE);
    p.text(
        chip.center(),
        Align2::CENTER_CENTER,
        crate::series::TF_LABELS[opts.tf],
        FontId::monospace(9.5),
        th::MA20,
    );

    let c = match candle {
        Some(c) => c,
        None => return,
    };

    let big = p.text(
        egui::pos2(x, rect.top() + y_big),
        Align2::LEFT_TOP,
        fmt_num(c.c, opts.decimals).as_str(),
        FontId::monospace(if tight { 20.0 } else { 25.0 }),
        th::TEXT,
    );
    let mut nx = big.right() + 7.0;
    if opts.unit.is_empty() {
        nx += 12.0;
    } else {
        let r2 = p.text(
            egui::pos2(nx, rect.top() + y_unit),
            Align2::LEFT_TOP,
            opts.unit,
            FontId::proportional(if tight { 11.0 } else { 12.0 }),
            th::MUTED,
        );
        nx = r2.right() + 12.0;
    }
    let delta = c.c - c.o;
    let sign = if delta > 0.0 { "+" } else { "" };
    let s = format!("{}{}", sign, fmt_num(delta, opts.decimals));
    p.text(
        egui::pos2(nx, rect.top() + y_delta),
        Align2::LEFT_TOP,
        s.as_str(),
        FontId::monospace(if tight { 10.0 } else { 11.0 }),
        candle_color(&c),
    );

    // The OHLC row is a fixed rhythm of "label value" pairs. On a narrow pane
    // it used to run under the process panel and come back as half a character,
    // so each pair is dropped as soon as it would not fit whole.
    let pair_w = if tight { 48.0 } else { 58.0 };
    let f_pair = if tight { 9.0 } else { 10.0 };
    let right_limit = rect.right() - 8.0;
    let yy = rect.top() + y_ohlc;
    let mut ox = x;
    for (label, value) in [("O", c.o), ("H", c.h), ("L", c.l), ("C", c.c)] {
        if ox + pair_w > right_limit {
            return;
        }
        let r1 = p.text(
            egui::pos2(ox, yy),
            Align2::LEFT_TOP,
            label,
            FontId::monospace(f_pair),
            th::DIM,
        );
        let r2 = p.text(
            egui::pos2(r1.right() + 4.0, yy),
            Align2::LEFT_TOP,
            fmt_num(value, opts.decimals).as_str(),
            FontId::monospace(f_pair),
            th::MUTED,
        );
        ox = r2.right() + 14.0;
    }

    let idx = hovered.unwrap_or_else(|| dq.len().saturating_sub(1));
    for (period, colour) in [(5usize, th::MA5), (20usize, th::MA20)] {
        if ox + pair_w > right_limit {
            return;
        }
        let label = format!("MA{}", period);
        let r1 = p.text(
            egui::pos2(ox, yy),
            Align2::LEFT_TOP,
            label.as_str(),
            FontId::monospace(f_pair),
            th::DIM,
        );
        let value = Series::ma(dq, idx, period)
            .map(|v| fmt_num(v, opts.decimals))
            .unwrap_or_else(|| "--".to_string());
        let r2 = p.text(
            egui::pos2(r1.right() + 4.0, yy),
            Align2::LEFT_TOP,
            value.as_str(),
            FontId::monospace(f_pair),
            colour,
        );
        ox = r2.right() + 14.0;
    }
}

fn draw_ma(
    p: &egui::Painter,
    dq: &VecDeque<Candle>,
    i0: usize,
    i1: usize,
    period: usize,
    x_of: &dyn Fn(f32) -> f32,
    y_of: &dyn Fn(f32) -> f32,
    color: Color32,
) {
    let mut pts: Vec<Pos2> = Vec::new();
    for i in i0..i1 {
        match Series::ma(dq, i, period) {
            Some(m) => pts.push(egui::pos2(x_of(i as f32 + 0.5), y_of(m))),
            None => flush(&mut pts, p, color),
        }
    }
    flush(&mut pts, p, color);
}

fn flush(pts: &mut Vec<Pos2>, p: &egui::Painter, color: Color32) {
    if pts.len() >= 2 {
        let line = std::mem::take(pts);
        p.add(egui::Shape::line(line, Stroke::new(1.4_f32, color)));
    } else {
        pts.clear();
    }
}

fn dashed_hline(p: &egui::Painter, x0: f32, x1: f32, y: f32, stroke: Stroke) {
    let mut x = x0;
    while x < x1 {
        let xe = (x + 5.0).min(x1);
        p.line_segment([egui::pos2(x, y), egui::pos2(xe, y)], stroke);
        x += 9.0;
    }
}

/// Digits needed so two neighbouring ticks never print the same string.
fn tick_decimals(step: f32) -> usize {
    if !step.is_finite() || step <= 0.0 {
        return 1;
    }
    (-step.log10()).ceil().max(0.0) as usize
}

fn nice_step(range: f32, target: f32) -> f32 {
    if !range.is_finite() || range <= 0.0 {
        return 1.0;
    }
    let raw = range / target.max(1.0);
    let mag = 10f32.powf(raw.log10().floor());
    if !mag.is_finite() || mag <= 0.0 {
        return 1.0;
    }
    let norm = raw / mag;
    let mult = if norm <= 1.5 {
        1.0
    } else if norm <= 3.0 {
        2.0
    } else if norm <= 7.0 {
        5.0
    } else {
        10.0
    };
    mult * mag
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_holds_a_scrolled_back_window() {
        let mut v = ChartView::new();
        v.offset = 50.0;
        v.anchor(1800, 0); // prime, must not jump
        assert_eq!(v.offset, 50.0);

        // Buffer still filling: one new candle, nothing evicted yet.
        v.anchor(1801, 0);
        assert_eq!(v.offset, 51.0);

        // Buffer saturated: one in, one out, n unchanged.
        v.anchor(1801, 1);
        assert_eq!(v.offset, 52.0);
    }

    #[test]
    fn anchor_never_moves_the_live_view() {
        let mut v = ChartView::new();
        v.anchor(100, 0);
        v.anchor(120, 0);
        v.anchor(1800, 20);
        assert_eq!(v.offset, 0.0);
    }

    #[test]
    fn wheel_zoom_responds_from_the_visible_window() {
        // Fresh buffer: ten candles on screen, the stored preference still
        // at its initial 150. One zoom-in notch has to shrink what is
        // visible immediately; acting on the preference would spend ~37
        // notches in the invisible range first.
        assert!(zoom_step(150.0, 10.0, 50.0, 420.0) < 10.0);
        // Zoom-out grows the preference, so "show everything" stays sticky
        // while the buffer fills.
        assert!(zoom_step(150.0, 10.0, -50.0, 420.0) > 150.0);
        // The 8-candle floor and the density cap still hold.
        assert_eq!(zoom_step(8.0, 8.0, 50.0, 420.0), 8.0);
        assert_eq!(zoom_step(420.0, 420.0, -50.0, 420.0), 420.0);
    }

    #[test]
    fn flat_candles_are_neutral_not_red() {
        let flat = Candle {
            t: 0,
            o: 5.0,
            h: 5.0,
            l: 5.0,
            c: 5.0,
            v: 0.0,
        };
        // A doji is neither up nor down: neutral, never the "up" red.
        assert_eq!(candle_color(&flat), th::FLAT);
        assert_eq!(candle_color(&Candle { c: 5.5, h: 5.5, ..flat }), th::UP);
        assert_eq!(candle_color(&Candle { c: 4.5, l: 4.5, ..flat }), th::DOWN);
    }
}
