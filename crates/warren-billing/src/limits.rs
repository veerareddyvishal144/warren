//! Wallet-derived workspace rules: the awake-instance ladder, the top-up bounds
//! and the balance required to create an instance. Pure functions.

use warren_core::{MICROS_PER_DOLLAR, Money};

/// Smallest accepted top-up: 5 USD.
pub const MIN_TOP_UP_MICROS: Money = 5 * MICROS_PER_DOLLAR;
/// Largest accepted top-up: 10,000 USD.
pub const MAX_TOP_UP_MICROS: Money = 10_000 * MICROS_PER_DOLLAR;

/// Awake instances allowed before any top-up.
pub const BASE_AWAKE_LIMIT: u32 = 3;

/// The ladder as `(cumulative top-ups in micros, awake limit)`, lowest rung
/// first. A workspace gets the limit of the highest rung it has reached. The
/// first rung is one micro, i.e. any top-up at all.
pub const AWAKE_LADDER: [(Money, u32); 4] = [
    (1, 10),
    (50 * MICROS_PER_DOLLAR, 50),
    (250 * MICROS_PER_DOLLAR, 200),
    (1_000 * MICROS_PER_DOLLAR, 1_000),
];

/// Days the wallet must cover at the running rate before an instance may be created.
pub const CREATE_RUNWAY_DAYS: Money = 30;

/// Awake-instance limit for a workspace whose cumulative top-ups are
/// `top_up_total_micros`: 3 before any top-up, 10 after the first, 50 from
/// 50 USD, 200 from 250 USD and 1,000 from 1,000 USD. Credits never climb the
/// ladder; only `Workspace::top_up_total_micros` does.
pub fn awake_limit(top_up_total_micros: Money) -> u32 {
    AWAKE_LADDER
        .iter()
        .rev()
        .find(|(threshold, _)| top_up_total_micros >= *threshold)
        .map(|(_, limit)| *limit)
        .unwrap_or(BASE_AWAKE_LIMIT)
}

/// Whether a top-up amount is within `MIN_TOP_UP_MICROS..=MAX_TOP_UP_MICROS`.
pub fn top_up_in_bounds(micros: Money) -> bool {
    (MIN_TOP_UP_MICROS..=MAX_TOP_UP_MICROS).contains(&micros)
}

/// Balance a wallet must hold to create an instance whose running price is
/// `monthly_running_micros` per month: one day of that rate (integer division;
/// the month is treated as 30 days for this rule).
pub fn required_balance_to_create(monthly_running_micros: Money) -> Money {
    monthly_running_micros.max(0) / CREATE_RUNWAY_DAYS
}

#[cfg(test)]
mod tests {
    use super::*;

    const USD: Money = MICROS_PER_DOLLAR;

    #[test]
    fn ladder_rungs() {
        assert_eq!(awake_limit(0), 3);
        assert_eq!(awake_limit(-1), 3);
        assert_eq!(awake_limit(1), 10);
        assert_eq!(awake_limit(5 * USD), 10);
        assert_eq!(awake_limit(50 * USD - 1), 10);
        assert_eq!(awake_limit(50 * USD), 50);
        assert_eq!(awake_limit(250 * USD - 1), 50);
        assert_eq!(awake_limit(250 * USD), 200);
        assert_eq!(awake_limit(1_000 * USD - 1), 200);
        assert_eq!(awake_limit(1_000 * USD), 1_000);
        assert_eq!(awake_limit(Money::MAX), 1_000);
    }

    #[test]
    fn top_up_bounds() {
        assert!(!top_up_in_bounds(0));
        assert!(!top_up_in_bounds(-5 * USD));
        assert!(!top_up_in_bounds(5 * USD - 1));
        assert!(top_up_in_bounds(5 * USD));
        assert!(top_up_in_bounds(10_000 * USD));
        assert!(!top_up_in_bounds(10_000 * USD + 1));
    }

    #[test]
    fn one_day_of_the_running_rate() {
        assert_eq!(required_balance_to_create(4_760_000), 158_666);
        assert_eq!(required_balance_to_create(0), 0);
        assert_eq!(required_balance_to_create(-30), 0);
    }
}
