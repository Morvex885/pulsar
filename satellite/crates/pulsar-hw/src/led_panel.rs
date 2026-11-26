//! Индикация на LED-панели Станции 2: состояния голоса, громкость, разовые сигналы и светомузыка,
//! нарисованные по геометрии панели, а не по кольцу.
//!
//! Кольцевые анимации (`led_fx::voice`, `volume`, `music` …) считают светодиоды по кругу в порядке
//! индексов. У панели индексы идут построчно по шахматной сетке (`PANEL_XY`), поэтому на ней
//! кольцевые волны превращаются в мелькающие полосы. Здесь всё строится вокруг логотипа, как у
//! штатной индикации панели: главный элемент — контур-«стадион» (прямоугольник со скруглёнными
//! концами; полудлина 3 и радиус 2 в единицах `PANEL_XY` дают ровно 14 светодиодов контура) с
//! мягким ореолом снаружи. Логотип (индексы 84..87) эти функции гасят: красный mute поверх
//! рисует компоновщик.
//! см. docs/station2.md, «Светодиоды и часы»
//!
//! Панель не добавляет собственных `mul_add`; общие функции сохраняют арифметику кольца.
//! см. docs/architecture.md, «Плавающая точка (FMA)»
//!
//! `out` — светодиоды панели (84 или 88 с логотипом). `t` у состояний и разовых сигналов — мс от
//! их начала, как у `led_fx::voice`; у светомузыки — фаза в секундах, как у `led_fx::music`.

use std::sync::LazyLock;

use crate::led_fx::{self as fx, Col, PANEL_XY};
pub(crate) use crate::led_fx::{PANEL_LEDS, PANEL_LOGO_LEDS};

const CX: f32 = 7.0;
const CY: f32 = 6.0;
/// Полудлина прямого участка и радиус скругления контура.
const HALF: f32 = 3.0;
const RAD: f32 = 2.0;
const PI: f32 = std::f32::consts::PI;
const TAU: f32 = std::f32::consts::TAU;
/// Длина контура, в тех же единицах.
const PERIMETER: f32 = 4.0 * HALF + TAU * RAD;
// Столбцы контура неравномерны: громкость заполняет пары по их порядку, а не по x.
const VOLUME_COLS: [f32; 7] = [-5.0, -4.0, -2.0, 0.0, 2.0, 4.0, 5.0];
const SPECTRUM_BANDS: usize = 8;
const VIRTUAL_RING: usize = 24;

// Фиолетовый — между кольцевыми VIOLET и VIOLET_SMALL `led_fx`: на панели под рассеивателем
// кольцевой оттенок смотрится синее и холоднее. Красный и зелёный — кольцевые (`led_fx`).
const VIOLET: u32 = 0x5a12ff;
/// Тусклый красный логотипа при выключенных микрофонах: ровный, не отвлекает.
const MUTE_LOGO: u32 = 0x500000;

/// Геометрия одного светодиода относительно контура.
#[derive(Clone, Copy)]
struct Geo {
    #[cfg(test)]
    dx: f32,
    dy: f32,
    /// Положение ближайшей точки контура, доля обхода 0..1 по часовой стрелке от верхней
    /// середины.
    s: f32,
    contour: f32,
    halo: f32,
    from_top: f32,
    pulse_dist: f32,
    volume_rank: f32,
    band: usize,
    band_offset: f32,
    ring_at: [usize; 2],
    ring_frac: f32,
    ring_weight: f32,
}

static GEOMETRY: LazyLock<[Geo; PANEL_LEDS]> = LazyLock::new(|| {
    let mut base = [f32::MAX; SPECTRUM_BANDS];
    let mut points = std::array::from_fn(|i| {
        let (col, row) = PANEL_XY[i];
        let dx = f32::from(col) - CX;
        let dy = f32::from(row) - CY;
        let sd = (dx.abs() - HALF).max(0.0).hypot(dy) - RAD;
        let s = contour_pos(dx, dy) / PERIMETER;
        let contour = 1.0 - fx::smoothstep(0.45, 0.95, sd.abs());
        let halo = if sd <= 0.6 {
            0.0
        } else {
            1.0 - fx::smoothstep(0.75, 2.4, sd)
        };
        let volume_rank = if contour > 0.0 {
            VOLUME_COLS
                .iter()
                .position(|&x| (dx - x).abs() < 0.5)
                .expect("светодиод контура вне столбцов громкости") as f32
        } else {
            0.0
        };
        let band = (dx.abs().round() as usize).min(SPECTRUM_BANDS - 1);
        base[band] = base[band].min(dy.abs());
        // Угол с поправкой на вытянутость панели; 0 — вверх.
        let a = (dx / 1.6).atan2(-dy).rem_euclid(TAU) / TAU * VIRTUAL_RING as f32;
        let whole = a.floor();
        let i0 = whole as usize % VIRTUAL_RING;
        Geo {
            #[cfg(test)]
            dx,
            dy,
            s,
            contour,
            halo,
            from_top: sdist(s, 0.0),
            // Весь контур на расстоянии 0, иначе в тишине он светится пятнами.
            pulse_dist: sd * (1.0 - contour),
            volume_rank,
            band,
            band_offset: 0.0,
            ring_at: [i0, (i0 + 1) % VIRTUAL_RING],
            ring_frac: a - whole,
            ring_weight: 1.0 - fx::smoothstep(0.6, 3.6, sd),
        }
    });
    // В шахматной сетке ближайший к средней линии светодиод у каждой полосы свой.
    for g in &mut points {
        g.band_offset = g.dy.abs() - base[g.band];
    }
    points
});

/// Длина дуги контура от верхней середины по часовой стрелке (y растёт вниз) до точки контура,
/// ближайшей к (dx, dy).
fn contour_pos(dx: f32, dy: f32) -> f32 {
    if dx.abs() <= HALF {
        if dy < 0.0 {
            // Верхний участок: от середины вправо, левая половина — в конце обхода.
            if dx >= 0.0 { dx } else { PERIMETER + dx }
        } else {
            // Нижний участок справа налево.
            HALF + PI * RAD + (HALF - dx)
        }
    } else if dx > 0.0 {
        // Правое скругление: угол от −90° (верх) до +90° (низ).
        let th = dy.atan2(dx - HALF);
        HALF + RAD * (th + PI / 2.0)
    } else {
        // Левое скругление: угол от +90° (низ) через 180° до 270° (верх).
        let mut ph = dy.atan2(dx + HALF);
        if ph < PI / 2.0 {
            ph += TAU;
        }
        3.0 * HALF + PI * RAD + RAD * (ph - PI / 2.0)
    }
}

/// Круговое расстояние между долями обхода.
fn sdist(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(1.0);
    d.min(1.0 - d)
}

/// `f` для каждого светодиода панели; логотип и всё дальше — тёмные.
fn each(out: &mut [Col], mut f: impl FnMut(&Geo) -> Col) {
    for (o, g) in out.iter_mut().zip(GEOMETRY.iter()) {
        *o = f(g);
    }
    for o in out.iter_mut().skip(PANEL_LEDS) {
        *o = Col::default();
    }
}

/// Плавное колебание 0..1 с периодом `period` мс, начиная с максимума.
fn swell(t: f32, period: f32) -> f32 {
    0.5 * (TAU * (t % period) / period).cos() + 0.5
}

// ---------------------------------------------------------------------------------------------
// Голос

pub fn voice(v: fx::Voice, t: i64, out: &mut [Col]) {
    let ft = t as f32;
    match v {
        fx::Voice::Listening => {
            // Контур поднимается снизу за 450 мс, затем спокойно дышит (100 % ↔ 65 %, 2,2 с),
            // ореол — глубже.
            const RISE: f32 = 450.0;
            if ft < RISE {
                // Фронт уходит за верхний ряд ореола; его вес совпадает с началом дыхания.
                let front = 4.8 - 9.8 * ft / RISE;
                each(out, |g| {
                    let k = fx::smoothstep(front - 0.8, front + 0.8, g.dy);
                    fx::stock(VIOLET, k * (g.contour + 0.35 * g.halo))
                });
            } else {
                let b = swell(ft - RISE, 2200.0);
                let contour_gain = 0.35 * b + 0.65;
                each(out, |g| {
                    fx::stock(VIOLET, g.contour * contour_gain + g.halo * 0.35 * b)
                });
            }
        }
        fx::Voice::Thinking => {
            // Дуга из ~4 светодиодов с мягким хвостом обходит контур за 1,5 с.
            let head = (t % 1500) as f32 / 1500.0;
            each(out, |g| {
                if g.contour <= 0.0 {
                    return Col::default();
                }
                let behind = (head - g.s).rem_euclid(1.0) * PERIMETER;
                let ahead = (g.s - head).rem_euclid(1.0) * PERIMETER;
                let k = if behind <= 8.5 {
                    1.0 - fx::smoothstep(4.5, 8.5, behind)
                } else {
                    1.0 - fx::smoothstep(0.0, 1.4, ahead)
                };
                fx::stock(VIOLET, g.contour * k)
            });
        }
        fx::Voice::Speaking => {
            // Ровный контур, яркость чуть колышется; по ореолу медленно бежит мягкая волна.
            let base = 0.12 * (TAU * ft / 1300.0).sin() + 0.88;
            each(out, |g| {
                let w = 0.5 * (TAU * (g.s * 2.0 - ft / 1800.0)).sin() + 0.5;
                fx::stock(VIOLET, g.contour * base + g.halo * 0.45 * w * w)
            });
        }
        fx::Voice::Error => {
            // Три вспышки контура, тайминг кольцевой Error.
            let k = fx::error_k(t);
            each(out, |g| fx::stock(fx::ERROR_RED, k * g.contour));
        }
        fx::Voice::TimerRing => {
            // Зелёный контур и широкий ореол дышат с периодом 2,4 с.
            let b = swell(ft, 2400.0);
            each(out, |g| {
                fx::stock(
                    fx::TIMER_GREEN,
                    (0.75 * b + 0.25) * g.contour + 0.5 * b * g.halo,
                )
            });
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Громкость и разовые сигналы

/// Цвет уровня громкости: зелёный до 60 %, затем через жёлтый к красному.
fn volume_colour(v: f32) -> Col {
    let hue = if v <= 0.6 {
        120.0
    } else {
        120.0 * (1.0 - (v - 0.6) / 0.4)
    };
    fx::hsv(hue / 360.0, 1.0, 1.0)
}

/// Контур заполняется слева направо до уровня `v`; незаполненная часть еле видна.
pub fn volume(v: f32, out: &mut [Col]) {
    let v = fx::clamp01(v);
    let col = volume_colour(v);
    let columns = v * VOLUME_COLS.len() as f32;
    each(out, |g| {
        if g.contour <= 0.0 {
            return Col::default();
        }
        let k = fx::clamp01(columns - g.volume_rank);
        col * (g.contour * (0.9 * k + 0.1))
    });
}

/// Таймер запущен: зелёный контур за `TIMER_STARTED_MS` сжимается к верхней середине.
pub fn timer_started(t: i64, out: &mut [Col]) {
    let left = 1.0 - t.min(fx::TIMER_STARTED_MS) as f32 / fx::TIMER_STARTED_MS as f32;
    let fade = (left * 15.0).min(1.0);
    // Дальше половины обхода на ширину края, как у `mute_sweep`: в начале контур замкнут.
    let reach = left * 0.53;
    each(out, |g| {
        let k = fx::smoothstep(reach + 0.03, reach - 0.03, g.from_top);
        fx::stock(fx::TIMER_START_GREEN, g.contour * k * fade)
    });
}

/// Микрофоны выключили — красный контур расходится от верха вниз; включили — расходится снизу
/// вверх и гаснет.
pub fn mute_sweep(on: bool, t: i64, out: &mut [Col]) {
    let ft = t.min(fx::MUTE_SWEEP_MS) as f32;
    // Дальше половины обхода на ширину края: иначе самый дальний светодиод остаётся на
    // полуяркости, и контур не смыкается.
    let reach = (ft / 450.0).min(1.0) * 0.54;
    let tail = if ft > 450.0 {
        1.0 - (ft - 450.0) / (fx::MUTE_SWEEP_MS as f32 - 450.0)
    } else {
        1.0
    };
    each(out, |g| {
        let d = if on { g.from_top } else { 0.5 - g.from_top };
        let k = fx::smoothstep(reach + 0.04, reach - 0.04, d);
        fx::stock(fx::MUTE_RED, g.contour * k * tail)
    });
}

/// Ровная индикация выключенных микрофонов — на логотипе.
pub fn mute_logo() -> Col {
    fx::stock(MUTE_LOGO, 1.0)
}

// ---------------------------------------------------------------------------------------------
// Светомузыка

/// Светомузыка по геометрии панели. Disco и Polar кольцевые по природе: их рисует
/// `led_fx::music` на виртуальном кольце, а сюда они попадают по углу вокруг логотипа.
pub fn music(s: fx::Style, st: &fx::MusicState, inp: &fx::MusicInput, t: f64, out: &mut [Col]) {
    match s {
        fx::Style::Spectrum => {
            // Столбики от средней линии вверх и вниз: басы в середине, высокие частоты по краям.
            let hue = fx::palette(t);
            let bands = std::array::from_fn::<_, SPECTRUM_BANDS, _>(|band| {
                let lvl = fx::clamp01(inp.bands[band]);
                let reach = lvl * 6.6;
                // У края остаётся полуяркость; этот множитель гасит полосу в тишине.
                let gain = fx::smoothstep(0.0, 0.05, lvl);
                let color = fx::hsv(
                    ((25.0 * band as f32 + hue) / 360.0) % 1.0,
                    1.0,
                    lvl.max(0.2),
                );
                (reach, gain, color)
            });
            each(out, |g| {
                let (reach, gain, color) = bands[g.band];
                let k = fx::smoothstep(reach + 0.6, reach - 0.6, g.band_offset) * gain;
                color * k
            });
        }
        fx::Style::Pulse => {
            // Стадион, разрастающийся от контура с громкостью; на долю — короткая подсветка.
            let loud = fx::clamp01(inp.loudness);
            let v = (0.3 * inp.flash + 0.85 * loud + 0.15).min(1.0);
            // До дальних углов панели (до контура ~4,7) на полной громкости.
            let reach = loud * 5.1;
            let col = fx::hsv(fx::palette(t) / 360.0, 0.9, v);
            each(out, |g| {
                col * fx::smoothstep(reach + 0.7, reach - 0.3, g.pulse_dist)
            });
        }
        fx::Style::Disco | fx::Style::Polar => {
            let mut ring = [Col::default(); VIRTUAL_RING];
            fx::music(s, st, inp, t, &mut ring);
            each(out, |g| {
                let [a, b] = g.ring_at;
                fx::lerp(ring[a], ring[b], g.ring_frac) * g.ring_weight
            });
        }
    }
}

#[cfg(test)]
mod tests;
