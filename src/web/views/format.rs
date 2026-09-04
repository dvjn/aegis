//! Number formatting shared by the overview tiles, tables, and charts.

pub(crate) fn count_text(value: i64) -> String {
    let digits = value.abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if value < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

pub(crate) fn money_text(nanodollars: i64) -> String {
    const NANODOLLARS_PER_CENT: i64 = 10_000_000;
    let dollars = nanodollars as f64 / crate::pricing::NANODOLLARS_PER_DOLLAR;
    if nanodollars == 0 {
        "$0.00".to_owned()
    } else if nanodollars.abs() < NANODOLLARS_PER_CENT {
        format!("${dollars:.4}")
    } else {
        let cents = (dollars * 100.0).round() as i64;
        format!("${}.{:02}", count_text(cents / 100), (cents % 100).abs())
    }
}

pub(crate) fn token_text(value: i64) -> String {
    match value {
        0 => "0".to_owned(),
        value if value < 10_000 => count_text(value),
        value if value < 1_000_000 => format!("{:.1}k", value as f64 / 1_000.0),
        value => format!("{:.1}M", value as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::{count_text, money_text, token_text};

    #[test]
    fn numbers_read_as_summaries() {
        for (value, expected) in [(0, "0"), (42, "42"), (1_284, "1,284"), (350_709, "350,709")] {
            assert_eq!(count_text(value), expected, "count {value}");
        }
        for (value, expected) in [
            (0, "0"),
            (9_999, "9,999"),
            (10_000, "10.0k"),
            (350_709, "350.7k"),
            (4_200_000, "4.2M"),
        ] {
            assert_eq!(token_text(value), expected, "tokens {value}");
        }
    }

    #[test]
    fn money_keeps_fractions_of_a_cent_visible() {
        for (nanodollars, expected) in [
            (0, "$0.00"),
            (1_000, "$0.0000"),
            (4_700_000, "$0.0047"),
            (10_000_000, "$0.01"),
            (39_139_738_410, "$39.14"),
            (1_234_567_890_000, "$1,234.57"),
        ] {
            assert_eq!(money_text(nanodollars), expected, "cost {nanodollars}");
        }
    }
}
