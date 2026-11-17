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
//! Побитового эталона у этой отрисовки нет — `mul_add` не используется.
//! см. docs/architecture.md, «Плавающая точка (FMA)»
//!
//! `out` — светодиоды панели (84 или 88 с логотипом). `t` у состояний и разовых сигналов — мс от
//! их начала, как у `led_fx::voice`; у светомузыки — фаза в секундах, как у `led_fx::music`.

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

// Фиолетовый — между кольцевыми VIOLET и VIOLET_SMALL `led_fx`: на панели под рассеивателем
// кольцевой оттенок смотрится синее и холоднее. Красный и зелёный — кольцевые (`led_fx`).
const VIOLET: u32 = 0x5a12ff;
/// Тусклый красный логотипа при выключенных микрофонах: ровный, не отвлекает.
const MUTE_LOGO: u32 = 0x500000;

/// Геометрия одного светодиода относительно контура.
#[derive(Clone, Copy, Debug)]
struct Geo {
    dx: f32,
    dy: f32,
    /// Расстояние со знаком до контура: < 0 внутри, > 0 снаружи.
    sd: f32,
    /// Положение ближайшей точки контура, доля обхода 0..1 по часовой стрелке от верхней
    /// середины.
    s: f32,
}

fn geo(i: usize) -> Geo {
    let (col, row) = PANEL_XY[i];
    let dx = f32::from(col) - CX;
    let dy = f32::from(row) - CY;
    let sd = (dx.abs() - HALF).max(0.0).hypot(dy) - RAD;
    Geo {
        dx,
        dy,
        sd,
        s: contour_pos(dx, dy) / PERIMETER,
    }
}

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

/// Вес «на контуре»: 1 на ряду светодиодов контура, 0 через ряд.
fn on_contour(g: &Geo) -> f32 {
    1.0 - fx::smoothstep(0.45, 0.95, g.sd.abs())
}

/// Вес ореола: ряд снаружи контура, дальше гаснет.
fn halo(g: &Geo) -> f32 {
    if g.sd <= 0.6 {
        0.0
    } else {
        1.0 - fx::smoothstep(0.75, 2.4, g.sd)
    }
}

/// Круговое расстояние между долями обхода.
fn sdist(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(1.0);
    d.min(1.0 - d)
}

/// `f` для каждого светодиода панели; логотип и всё дальше — тёмные.
fn each(out: &mut [Col], mut f: impl FnMut(&Geo) -> Col) {
    for (i, o) in out.iter_mut().take(PANEL_LEDS).enumerate() {
        *o = f(&geo(i));
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
            each(out, |g| {
                let c = on_contour(g);
                let h = halo(g);
                if ft < RISE {
                    // Фронт идёт снизу вверх и к концу уходит за верхний ряд ореола; вес ореола —
                    // как в начале дыхания, чтобы на стыке не было скачка.
                    let front = 4.8 - 9.8 * ft / RISE;
                    let k = fx::smoothstep(front - 0.8, front + 0.8, g.dy);
                    return fx::stock(VIOLET, k * (c + 0.35 * h));
                }
                let b = swell(ft - RISE, 2200.0);
                fx::stock(VIOLET, c * (0.35 * b + 0.65) + h * 0.35 * b)
            });
        }
        fx::Voice::Thinking => {
            // Дуга из ~4 светодиодов с мягким хвостом обходит контур за 1,5 с.
            let head = (t % 1500) as f32 / 1500.0;
            each(out, |g| {
                let c = on_contour(g);
                if c <= 0.0 {
                    return Col::default();
                }
                let behind = (head - g.s).rem_euclid(1.0) * PERIMETER;
                let ahead = (g.s - head).rem_euclid(1.0) * PERIMETER;
                let k = if behind <= 8.5 {
                    1.0 - fx::smoothstep(4.5, 8.5, behind)
                } else {
                    1.0 - fx::smoothstep(0.0, 1.4, ahead)
                };
                fx::stock(VIOLET, c * k)
            });
        }
        fx::Voice::Speaking => {
            // Ровный контур, яркость чуть колышется; по ореолу медленно бежит мягкая волна.
            let base = 0.12 * (TAU * ft / 1300.0).sin() + 0.88;
            each(out, |g| {
                let c = on_contour(g);
                let h = halo(g);
                let w = 0.5 * (TAU * (g.s * 2.0 - ft / 1800.0)).sin() + 0.5;
                fx::stock(VIOLET, c * base + h * 0.45 * w * w)
            });
        }
        fx::Voice::Error => {
            // Три вспышки контура, тайминг кольцевой Error.
            let k = fx::error_k(t);
            each(out, |g| fx::stock(fx::ERROR_RED, k * on_contour(g)));
        }
        fx::Voice::TimerRing => {
            // Зелёный контур и широкий ореол дышат с периодом 2,4 с.
            let b = swell(ft, 2400.0);
            each(out, |g| {
                fx::stock(
                    fx::TIMER_GREEN,
                    (0.75 * b + 0.25) * on_contour(g) + 0.5 * b * halo(g),
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
    // Столбцы контура слева направо. Заполнение идёт по номеру столбца, а не по x: столбцы
    // стоят неравномерно, и по x на круглых значениях громкости соседняя пара горела вполсилы.
    // Громкость 0 — ни одного светодиода, 1 — все.
    const COLS: [f32; 7] = [-5.0, -4.0, -2.0, 0.0, 2.0, 4.0, 5.0];
    each(out, |g| {
        let c = on_contour(g);
        if c <= 0.0 {
            return Col::default();
        }
        let rank = COLS
            .iter()
            .position(|&x| (g.dx - x).abs() < 0.5)
            .unwrap_or(0);
        let k = fx::clamp01(v * COLS.len() as f32 - rank as f32);
        col * (c * (0.9 * k + 0.1))
    });
}

/// Таймер запущен: зелёный контур за `TIMER_STARTED_MS` сжимается к верхней середине.
pub fn timer_started(t: i64, out: &mut [Col]) {
    let left = 1.0 - t.min(fx::TIMER_STARTED_MS) as f32 / fx::TIMER_STARTED_MS as f32;
    let fade = (left * 15.0).min(1.0);
    each(out, |g| {
        let d = sdist(g.s, 0.0);
        // Дальше половины обхода на ширину края, как у `mute_sweep`: в начале контур замкнут.
        let reach = left * 0.53;
        let k = fx::smoothstep(reach + 0.03, reach - 0.03, d);
        fx::stock(fx::TIMER_START_GREEN, on_contour(g) * k * fade)
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
        let mut d = sdist(g.s, 0.0);
        if !on {
            d = 0.5 - d;
        }
        let k = fx::smoothstep(reach + 0.04, reach - 0.04, d);
        fx::stock(fx::MUTE_RED, on_contour(g) * k * tail)
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
    let hue = fx::palette(t);
    let loud = fx::clamp01(inp.loudness);
    match s {
        fx::Style::Spectrum => {
            // Столбики от средней линии вверх и вниз: басы в середине, высокие частоты по краям.
            let b = &inp.bands;
            // Столбик каждой полосы растёт от её ближайшего к средней линии светодиода: в шахматной
            // сетке он у разных полос на разной высоте, и от общей линии басы загорались бы позже.
            let mut base = [f32::MAX; 8];
            for i in 0..PANEL_LEDS {
                let g = geo(i);
                let band = (g.dx.abs().round() as usize).min(7);
                base[band] = base[band].min(g.dy.abs());
            }
            each(out, |g| {
                let band = (g.dx.abs().round() as usize).min(7);
                let lvl = fx::clamp01(b[band]);
                let reach = lvl * 6.6;
                // Без звука в полосе — темно: у края столбика иначе остаётся полуяркость.
                let k = fx::smoothstep(reach + 0.6, reach - 0.6, g.dy.abs() - base[band])
                    * fx::smoothstep(0.0, 0.05, lvl);
                fx::hsv(
                    ((25.0 * band as f32 + hue) / 360.0) % 1.0,
                    1.0,
                    lvl.max(0.2),
                ) * k
            });
        }
        fx::Style::Pulse => {
            // Стадион, разрастающийся от контура с громкостью; на долю — короткая подсветка.
            let v = (0.3 * inp.flash + 0.85 * loud + 0.15).min(1.0);
            // До дальних углов панели (до контура ~4,7) на полной громкости.
            let reach = loud * 5.1;
            let col = fx::hsv(hue / 360.0, 0.9, v);
            // Весь ряд контура — расстояние 0: у части его светодиодов `sd` чуть больше нуля, и
            // в тишине контур светился бы пятнами.
            each(out, |g| {
                let d = g.sd * (1.0 - on_contour(g));
                col * fx::smoothstep(reach + 0.7, reach - 0.3, d)
            });
        }
        fx::Style::Disco | fx::Style::Polar => {
            const RING: usize = 24;
            let mut ring = [Col::default(); RING];
            fx::music(s, st, inp, t, &mut ring);
            each(out, |g| {
                // Угол вокруг центра с поправкой на вытянутость панели; 0 — вверх.
                let a = (g.dx / 1.6).atan2(-g.dy).rem_euclid(TAU) / TAU * RING as f32;
                let i0 = a.floor() as usize % RING;
                let f = a - a.floor();
                let c = fx::lerp(ring[i0], ring[(i0 + 1) % RING], f);
                c * (1.0 - fx::smoothstep(0.6, 3.6, g.sd))
            });
        }
    }
}

#[cfg(test)]
mod tests;
