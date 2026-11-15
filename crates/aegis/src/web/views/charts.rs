//! Hand-written inline SVG charts. Every function is pure: values in, markup
//! out. Colours come from CSS classes so the charts follow the page palette.

use maud::{Markup, html};

/// One line per series in a line chart. The label is shown in the legend.
#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    pub label: String,
    pub values: Vec<f64>,
}

/// The palette has this many series classes (`.series-1` .. `.series-8`).
const PALETTE_SIZE: usize = 8;

/// A line chart shows at most this many lines before the rest are summed
/// into an "others" line.
const MAX_LINES: usize = 6;

/// A non-zero stacked segment never drops below this share of the bar, so a
/// handful of failures stays visible beside thousands of successes.
const MIN_SEGMENT_SHARE: f64 = 1.5;

const LINE_CHART_WIDTH: f64 = 800.0;
/// Two thirds of the earlier 220, so the table under the chart gets the room;
/// the y tick labels keep their size because the SVG scales with its width.
const LINE_CHART_HEIGHT: f64 = 150.0;
const LINE_CHART_LEFT: f64 = 56.0;
const LINE_CHART_RIGHT: f64 = 16.0;
const LINE_CHART_TOP: f64 = 12.0;
const LINE_CHART_BOTTOM: f64 = 12.0;
const Y_TICKS: usize = 3;

pub fn sparkline(values: &[f64], width: u32, height: u32) -> Markup {
    let width_f = f64::from(width);
    let height_f = f64::from(height);
    let max = values.iter().copied().fold(0.0_f64, f64::max);
    let path = smooth_path(&plot_points(values, 0.0, width_f, 0.0, height_f, max));
    html! {
        svg class="sparkline" viewBox=(format!("0 0 {width} {height}")) width=(width) height=(height)
            preserveAspectRatio="none" aria-hidden="true" focusable="false" {
            path d=(path) fill="none" stroke="currentColor" stroke-width="2.5"
                stroke-linejoin="round" stroke-linecap="round" vector-effect="non-scaling-stroke" {}
        }
    }
}

/// Segments are `(label, value, css class)`. Zero segments are left out of
/// the bar but listed in the legend, so the reader sees the category exists.
pub fn stacked_bar(segments: &[(&str, f64, &str)], format: &dyn Fn(f64) -> String) -> Markup {
    html! {
        div class="stacked" {
            (stacked_bar_without_legend(segments, format))
            ul class="stacked-legend" {
                @for (label, value, class) in segments {
                    li title=(format!("{} {label}", format(*value))) {
                        span class=(format!("swatch {class}")) aria-hidden="true" {}
                        span class="legend-value" { (format(*value)) }
                        " "
                        span class="legend-label" { (label) }
                    }
                }
            }
        }
    }
}

/// The bar alone, for a card whose table is the key.
pub fn stacked_bar_without_legend(
    segments: &[(&str, f64, &str)],
    format: &dyn Fn(f64) -> String,
) -> Markup {
    let shares = segment_shares(
        &segments
            .iter()
            .map(|(_, value, _)| *value)
            .collect::<Vec<_>>(),
    );
    let mut offset = 0.0;
    let placed: Vec<(f64, f64)> = shares
        .iter()
        .map(|share| {
            let start = offset;
            offset += share;
            (start, *share)
        })
        .collect();
    html! {
        svg class="stacked-bar" viewBox="0 0 100 8" preserveAspectRatio="none" aria-hidden="true" focusable="false" {
            @for ((label, value, class), (start, share)) in segments.iter().zip(&placed) {
                @if *share > 0.0 {
                    rect class=(class) x=(coordinate(*start)) y="0" width=(coordinate(*share)) height="8" {
                        title { (format(*value)) " " (label) }
                    }
                }
            }
        }
    }
}

/// Draws the top series as lines over light gridlines, with y tick labels
/// only. Series beyond the first five are summed into "others" once there
/// are more than six; the table beside the chart carries the key, coloured by
/// `row_series_class`.
pub fn line_chart(series: &[Series], format_y: &dyn Fn(f64) -> String) -> Markup {
    let series = limit_series(series);
    let max = series
        .iter()
        .flat_map(|line| line.values.iter().copied())
        .fold(0.0_f64, f64::max);
    let (tick_step, top) = nice_ticks(max, Y_TICKS);
    let plot_left = LINE_CHART_LEFT;
    let plot_right = LINE_CHART_WIDTH - LINE_CHART_RIGHT;
    let plot_top = LINE_CHART_TOP;
    let plot_bottom = LINE_CHART_HEIGHT - LINE_CHART_BOTTOM;

    html! {
        figure class="chart" {
            svg viewBox=(format!("0 0 {LINE_CHART_WIDTH} {LINE_CHART_HEIGHT}")) role="img"
                aria-label="Tokens per bucket by series" {
                @for tick in 0..=Y_TICKS {
                    @let value = tick as f64 * tick_step;
                    @let y = value_to_y(value, top, plot_top, plot_bottom);
                    line class="chart-grid" x1=(coordinate(plot_left)) x2=(coordinate(plot_right))
                        y1=(coordinate(y)) y2=(coordinate(y)) {}
                    text class="chart-tick" x=(coordinate(plot_left - 8.0)) y=(coordinate(y))
                        text-anchor="end" dominant-baseline="middle" { (format_y(value)) }
                }
                @for (index, line) in series.iter().enumerate() {
                    path class=(format!("chart-line {}", series_class(index)))
                        d=(smooth_path(&plot_points(&line.values, plot_left, plot_right, plot_top, plot_bottom, top)))
                        fill="none" stroke="currentColor" stroke-width="3" stroke-linejoin="round" stroke-linecap="round"
                        vector-effect="non-scaling-stroke" {}
                }
            }
            @if series.is_empty() {
                figcaption class="chart-empty" { "No traffic in this range." }
            }
        }
    }
}

pub fn series_class(index: usize) -> String {
    format!("series-{}", index % PALETTE_SIZE + 1)
}

/// The class of the line that the series at `index` (of `count`, ordered as
/// the chart orders them) is drawn with. Series folded into "others" share
/// that line's class.
pub fn row_series_class(index: usize, count: usize) -> String {
    if count > MAX_LINES && index >= MAX_LINES - 1 {
        series_class(MAX_LINES - 1)
    } else {
        series_class(index)
    }
}

fn limit_series(series: &[Series]) -> Vec<Series> {
    if series.len() <= MAX_LINES {
        return series.to_vec();
    }
    let keep = MAX_LINES - 1;
    let mut kept = series[..keep].to_vec();
    let length = series
        .iter()
        .map(|line| line.values.len())
        .max()
        .unwrap_or(0);
    let mut others = vec![0.0; length];
    for line in &series[keep..] {
        for (slot, value) in others.iter_mut().zip(&line.values) {
            *slot += value;
        }
    }
    kept.push(Series {
        label: "others".to_owned(),
        values: others,
    });
    kept
}

/// Maps a value onto the vertical span `[top, bottom]` of a plot, with zero
/// on the bottom edge. A chart with no data draws a flat line on that edge.
fn value_to_y(value: f64, max: f64, top: f64, bottom: f64) -> f64 {
    if max <= 0.0 {
        return bottom;
    }
    let share = (value / max).clamp(0.0, 1.0);
    bottom - share * (bottom - top)
}

fn index_to_x(index: usize, count: usize, left: f64, right: f64) -> f64 {
    if count <= 1 {
        return left;
    }
    left + (right - left) * index as f64 / (count - 1) as f64
}

/// Places each value on the plot. A lone value becomes a flat line across the
/// whole width, so a single-bucket window still draws something.
fn plot_points(
    values: &[f64],
    left: f64,
    right: f64,
    top: f64,
    bottom: f64,
    max: f64,
) -> Vec<(f64, f64)> {
    if values.len() == 1 {
        let y = value_to_y(values[0], max, top, bottom);
        return vec![(left, y), (right, y)];
    }
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            (
                index_to_x(index, values.len(), left, right),
                value_to_y(*value, max, top, bottom),
            )
        })
        .collect()
}

/// A cubic bezier path through the points with monotone (Fritsch-Carlson)
/// tangents. Monotone interpolation never overshoots its neighbours, so a
/// series that touches zero never dips below the baseline.
fn smooth_path(points: &[(f64, f64)]) -> String {
    let Some((first_x, first_y)) = points.first() else {
        return String::new();
    };
    let mut path = format!("M{},{}", coordinate(*first_x), coordinate(*first_y));
    if points.len() < 2 {
        return path;
    }
    let tangents = monotone_tangents(points);
    for (index, pair) in points.windows(2).enumerate() {
        let (x0, y0) = pair[0];
        let (x1, y1) = pair[1];
        let third = (x1 - x0) / 3.0;
        path.push_str(&format!(
            " C{},{} {},{} {},{}",
            coordinate(x0 + third),
            coordinate(y0 + tangents[index] * third),
            coordinate(x1 - third),
            coordinate(y1 - tangents[index + 1] * third),
            coordinate(x1),
            coordinate(y1)
        ));
    }
    path
}

fn monotone_tangents(points: &[(f64, f64)]) -> Vec<f64> {
    let count = points.len();
    let secants: Vec<f64> = points
        .windows(2)
        .map(|pair| {
            let (x0, y0) = pair[0];
            let (x1, y1) = pair[1];
            if x1 == x0 { 0.0 } else { (y1 - y0) / (x1 - x0) }
        })
        .collect();
    let mut tangents = vec![0.0; count];
    tangents[0] = secants[0];
    tangents[count - 1] = secants[count - 2];
    for index in 1..count - 1 {
        let before = secants[index - 1];
        let after = secants[index];
        // A local extremum gets a flat tangent, so the curve turns there
        // instead of overshooting.
        tangents[index] = if before * after <= 0.0 {
            0.0
        } else {
            // The harmonic mean keeps the tangent within the bounds that
            // preserve monotonicity between neighbours.
            let (left_x, _) = points[index - 1];
            let (mid_x, _) = points[index];
            let (right_x, _) = points[index + 1];
            let left_span = mid_x - left_x;
            let right_span = right_x - mid_x;
            let total = left_span + right_span;
            3.0 * total / ((total + right_span) / before + (total + left_span) / after)
        };
    }
    tangents
}

/// Shares of a 100-unit bar. Non-zero values get at least `MIN_SEGMENT_SHARE`;
/// the remaining width is split among the rest in proportion to their values.
fn segment_shares(values: &[f64]) -> Vec<f64> {
    let total: f64 = values.iter().filter(|value| **value > 0.0).sum();
    if total <= 0.0 {
        return vec![0.0; values.len()];
    }
    let proportional: Vec<f64> = values
        .iter()
        .map(|value| {
            if *value > 0.0 {
                value / total * 100.0
            } else {
                0.0
            }
        })
        .collect();
    let pinned: Vec<bool> = proportional
        .iter()
        .map(|share| *share > 0.0 && *share < MIN_SEGMENT_SHARE)
        .collect();
    let pinned_total = pinned.iter().filter(|pinned| **pinned).count() as f64 * MIN_SEGMENT_SHARE;
    let free_total: f64 = proportional
        .iter()
        .zip(&pinned)
        .filter(|(_, pinned)| !**pinned)
        .map(|(share, _)| share)
        .sum();
    proportional
        .iter()
        .zip(&pinned)
        .map(|(share, pinned)| {
            if *pinned {
                MIN_SEGMENT_SHARE
            } else if free_total > 0.0 {
                share * (100.0 - pinned_total) / free_total
            } else {
                0.0
            }
        })
        .collect()
}

/// Picks a tick step of 1, 2, or 5 times a power of ten so `ticks` steps
/// cover `max`. Returns `(step, top)`.
fn nice_ticks(max: f64, ticks: usize) -> (f64, f64) {
    if max <= 0.0 {
        return (1.0, ticks as f64);
    }
    let raw_step = max / ticks as f64;
    let magnitude = 10_f64.powf(raw_step.log10().floor());
    let step = [1.0, 2.0, 5.0, 10.0]
        .into_iter()
        .map(|factor| factor * magnitude)
        .find(|step| step * ticks as f64 >= max)
        .unwrap_or(10.0 * magnitude);
    (step, step * ticks as f64)
}

/// Two decimals keep the markup short; SVG user units never need more.
fn coordinate(value: f64) -> String {
    let text = format!("{value:.2}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-0" {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_map_onto_the_plot_height_with_zero_at_the_bottom() {
        assert_eq!(value_to_y(0.0, 100.0, 10.0, 210.0), 210.0);
        assert_eq!(value_to_y(100.0, 100.0, 10.0, 210.0), 10.0);
        assert_eq!(value_to_y(50.0, 100.0, 10.0, 210.0), 110.0);
        assert_eq!(
            value_to_y(0.0, 0.0, 10.0, 210.0),
            210.0,
            "no data sits on the bottom edge"
        );
    }

    #[test]
    fn tiny_segments_keep_a_visible_width() {
        let shares = segment_shares(&[9_990.0, 10.0, 0.0]);
        assert_eq!(shares[1], MIN_SEGMENT_SHARE);
        assert_eq!(shares[2], 0.0, "a zero segment takes no room");
        assert!((shares.iter().sum::<f64>() - 100.0).abs() < 1e-9);

        let even = segment_shares(&[1.0, 1.0]);
        assert_eq!(even, vec![50.0, 50.0]);
        assert_eq!(segment_shares(&[0.0, 0.0]), vec![0.0, 0.0]);
    }

    #[test]
    fn ticks_round_up_to_a_readable_step() {
        assert_eq!(nice_ticks(2_700.0, 3), (1_000.0, 3_000.0));
        assert_eq!(nice_ticks(120.0, 3), (50.0, 150.0));
        assert_eq!(nice_ticks(0.0, 3), (1.0, 3.0));
    }

    #[test]
    fn many_series_collapse_into_others() {
        let series: Vec<Series> = (0..8)
            .map(|index| Series {
                label: format!("m{index}"),
                values: vec![1.0, 2.0],
            })
            .collect();
        let limited = limit_series(&series);
        assert_eq!(limited.len(), MAX_LINES);
        assert_eq!(limited[5].label, "others");
        assert_eq!(limited[5].values, vec![3.0, 6.0]);
        assert_eq!(
            limit_series(&series[..6]).len(),
            6,
            "six series fit as they are"
        );
    }

    #[test]
    fn flat_input_still_draws_a_line() {
        let markup = sparkline(&[0.0, 0.0, 0.0], 72, 18).into_string();
        assert!(
            markup.contains("d=\"M0,18 C12,18 24,18 36,18 C48,18 60,18 72,18\""),
            "{markup}"
        );
        assert!(!markup.contains("fill=\"currentColor\""), "stroke only");
        assert_eq!(coordinate(12.5), "12.5");
        assert_eq!(coordinate(12.0), "12");
    }

    #[test]
    fn the_path_is_a_smooth_curve_that_never_dips_under_the_baseline() {
        let single = smooth_path(&plot_points(&[5.0], 0.0, 72.0, 0.0, 18.0, 5.0));
        assert_eq!(
            single, "M0,0 C24,0 48,0 72,0",
            "a lone value is a flat line"
        );
        assert_eq!(smooth_path(&[(3.0, 4.0)]), "M3,4");
        assert_eq!(smooth_path(&[]), "");

        let points = plot_points(&[0.0, 10.0, 0.0, 0.0], 0.0, 300.0, 0.0, 100.0, 10.0);
        let path = smooth_path(&points);
        assert!(path.starts_with("M0,100 C"), "{path}");
        assert_eq!(path.matches(" C").count(), 3, "one cubic per gap");

        // Every control point stays inside the plot, so the curve cannot
        // leave it either: a bezier lies within the hull of its controls.
        let numbers: Vec<f64> = path
            .split([' ', ',', 'M', 'C'])
            .filter(|part| !part.is_empty())
            .map(|part| part.parse().unwrap())
            .collect();
        for y in numbers.iter().skip(1).step_by(2) {
            assert!((0.0..=100.0).contains(y), "{y} in {path}");
        }
        let tangents = monotone_tangents(&points);
        assert_eq!(tangents[1], 0.0, "the peak has a flat tangent");
        assert_eq!(tangents[2], 0.0, "the return to zero has a flat tangent");
    }
}
