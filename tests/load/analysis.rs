const LINEAR_FIT_MINIMUM_R_SQUARED: f64 = 0.90;
const MINIMUM_POINTS_FOR_A_FIT: usize = 3;

const FLAT_RESPONSE_FRACTION: f64 = 0.10;

#[derive(Clone, Debug, Default)]
pub struct Fit {
    pub intercept: f64,
    pub slope: f64,
    pub r_squared: f64,
    pub points: Vec<(f64, f64)>,
    pub dropped: Vec<f64>,
}

impl Fit {
    pub fn count(&self) -> usize {
        self.points.len()
    }

    pub fn is_flat(&self) -> bool {
        if self.count() < MINIMUM_POINTS_FOR_A_FIT {
            return false;
        }
        let span = maximum(self.points.iter().map(|&(x, _)| x))
            - minimum(self.points.iter().map(|&(x, _)| x));
        let mean_y = self.points.iter().map(|&(_, y)| y).sum::<f64>() / self.count() as f64;
        (self.slope * span).abs() < mean_y.abs() * FLAT_RESPONSE_FRACTION
    }

    pub fn is_trustworthy(&self) -> bool {
        self.count() >= MINIMUM_POINTS_FOR_A_FIT && self.r_squared >= LINEAR_FIT_MINIMUM_R_SQUARED
    }

    pub fn describe(&self, x_unit: &str, y_unit: &str) -> String {
        if self.count() < 2 {
            return format!("not enough points to fit ({})", self.count());
        }
        let quality = if self.is_flat() {
            "  FLAT, no measurable dependence on this axis"
        } else if self.is_trustworthy() {
            ""
        } else {
            "  UNTRUSTWORTHY FIT, do not read the slope"
        };
        let departure = if self.dropped.is_empty() {
            String::new()
        } else {
            format!(
                ", non-linear above {} {x_unit} (dropped {} point(s))",
                trim_number(minimum(self.dropped.iter().copied())),
                self.dropped.len()
            )
        };
        format!(
            "intercept {:+.2} {y_unit}, slope {:+.4} {y_unit}/{x_unit}, R2 {:.3} over {} points{departure}{quality}",
            self.intercept,
            self.slope,
            self.r_squared,
            self.count()
        )
    }

    pub fn plain(&self, per: &str, unit: &str) -> String {
        if self.count() < 2 {
            return "not enough data points to say".to_string();
        }
        if self.is_flat() {
            return format!(
                "no measurable effect: the fitted {:+.4} {unit} per {per} moves the total less than {:.0}% across the measured range",
                self.slope,
                FLAT_RESPONSE_FRACTION * 100.0
            );
        }
        if !self.is_trustworthy() {
            return format!(
                "about {:.1} {unit} per {per}, but the points do not form a straight line (R2 {:.2}), so read it as a rough direction, not a rate",
                self.slope, self.r_squared
            );
        }
        let departure = if self.dropped.is_empty() {
            String::new()
        } else {
            format!(
                ", and it stops being straight above {}",
                trim_number(minimum(self.dropped.iter().copied()))
            )
        };
        let fixed = if self.intercept >= 1.0 {
            format!(", plus a fixed {:.0} {unit} every request", self.intercept)
        } else {
            ", with no fixed overhead on top".to_string()
        };
        format!(
            "{:.1} {unit} per {per}{fixed} (R2 {:.2}){departure}",
            self.slope, self.r_squared
        )
    }
}

pub fn fit_linear(points: &[(f64, f64)]) -> Fit {
    let pairs = points.to_vec();
    if pairs.len() < 2 {
        return Fit {
            points: pairs,
            ..Fit::default()
        };
    }

    let count = pairs.len() as f64;
    let mean_x = pairs.iter().map(|&(x, _)| x).sum::<f64>() / count;
    let mean_y = pairs.iter().map(|&(_, y)| y).sum::<f64>() / count;
    let covariance = pairs
        .iter()
        .map(|&(x, y)| (x - mean_x) * (y - mean_y))
        .sum::<f64>();
    let variance_x = pairs
        .iter()
        .map(|&(x, _)| (x - mean_x).powi(2))
        .sum::<f64>();
    if variance_x == 0.0 {
        return Fit {
            intercept: mean_y,
            points: pairs,
            ..Fit::default()
        };
    }

    let slope = covariance / variance_x;
    let intercept = mean_y - slope * mean_x;
    let residual = pairs
        .iter()
        .map(|&(x, y)| (y - (intercept + slope * x)).powi(2))
        .sum::<f64>();
    let total = pairs
        .iter()
        .map(|&(_, y)| (y - mean_y).powi(2))
        .sum::<f64>();
    let r_squared = if total == 0.0 {
        1.0
    } else {
        1.0 - residual / total
    };
    Fit {
        intercept,
        slope,
        r_squared,
        points: pairs,
        dropped: Vec::new(),
    }
}

pub fn fit_linear_region(points: &[(f64, f64)]) -> Fit {
    let mut ordered = points.to_vec();
    ordered.sort_by(|left, right| left.0.total_cmp(&right.0));
    let whole = fit_linear(&ordered);
    if whole.is_trustworthy() || whole.is_flat() || ordered.len() < MINIMUM_POINTS_FOR_A_FIT + 1 {
        return whole;
    }

    for end in (MINIMUM_POINTS_FOR_A_FIT..ordered.len()).rev() {
        let mut candidate = fit_linear(&ordered[..end]);
        if candidate.is_trustworthy() {
            candidate.dropped = ordered[end..].iter().map(|&(x, _)| x).collect();
            return candidate;
        }
    }
    whole
}

fn minimum(values: impl Iterator<Item = f64>) -> f64 {
    values.fold(f64::INFINITY, f64::min)
}

fn maximum(values: impl Iterator<Item = f64>) -> f64 {
    values.fold(f64::NEG_INFINITY, f64::max)
}

fn trim_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}
