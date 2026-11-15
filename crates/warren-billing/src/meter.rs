//! Pure metering arithmetic. Nothing here touches the store, so the rules can
//! be tested exhaustively and reused by anyone who wants to preview a charge.
//!
//! A month is [`MINUTES_PER_MONTH`] minutes (730 hours). Every tick charges the
//! whole minutes elapsed since the instance's `metered_through`, at the monthly
//! price of its current state divided by the minutes in a month. The charge is
//! rounded down to whole micros and the fraction is carried on the instance, so
//! over a month the charges sum to the monthly price within one micro.

use warren_core::{Instance, LedgerKind, MINUTES_PER_MONTH, Money, Rates};

/// One advance of an instance's meter, as computed by [`advance`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterStep {
    /// Whole minutes charged; always at least one.
    pub minutes: i64,
    /// Whole micros to debit; zero when the span is too cheap to reach a micro.
    pub charge_micros: Money,
    /// Fractional micros carried into the next step, in `[0, 1)`.
    pub carry_micros: f64,
    /// Unix seconds the meter has charged up to after this step.
    pub metered_through: i64,
}

/// Price per minute of a monthly price. Negative prices count as zero.
pub fn per_minute_micros(monthly_micros: Money) -> f64 {
    monthly_micros.max(0) as f64 / MINUTES_PER_MONTH
}

/// Which price an instance pays right now: the running rate while its status
/// holds compute, the disk rate while it is asleep, stopped or waking, nothing
/// otherwise (provisioning, failed, deleting, deleted).
pub fn billing_rate(rates: &Rates, inst: &Instance) -> Option<(LedgerKind, Money)> {
    if inst.status.bills_compute() {
        Some((
            LedgerKind::Compute,
            rates.monthly_running(inst.shape, inst.kind),
        ))
    } else if inst.status.bills_disk() {
        Some((LedgerKind::Disk, rates.monthly_disk(inst.shape)))
    } else {
        None
    }
}

/// Where this instance's meter stands: `metered_through`, or `meter_started_at`
/// when the lifecycle has not set `metered_through` yet. `None` when the
/// instance has never reached running and is therefore still free.
pub fn metered_from(inst: &Instance) -> Option<i64> {
    if inst.meter_started_at <= 0 {
        return None;
    }
    Some(if inst.metered_through > 0 {
        inst.metered_through
    } else {
        inst.meter_started_at
    })
}

/// Advance a meter from `metered_through` to `now` at `monthly_micros` per
/// month, folding in `carry_micros` from the previous step. Returns `None` when
/// less than a whole minute has elapsed (including when `now` is in the past),
/// in which case nothing changes. A non-finite or negative carry, which can only
/// come from corrupt storage, is treated as zero.
pub fn advance(
    monthly_micros: Money,
    carry_micros: f64,
    metered_through: i64,
    now: i64,
) -> Option<MeterStep> {
    let minutes = now.saturating_sub(metered_through) / 60;
    if minutes <= 0 {
        return None;
    }
    let carry_in = if carry_micros.is_finite() && carry_micros >= 0.0 {
        carry_micros
    } else {
        0.0
    };
    let amount = minutes as f64 * per_minute_micros(monthly_micros) + carry_in;
    // `amount` is finite and non-negative here, so the cast cannot saturate in practice.
    let charge_micros = amount.floor() as Money;
    let carry_micros = amount - charge_micros as f64;
    Some(MeterStep {
        minutes,
        charge_micros,
        carry_micros,
        metered_through: metered_through + minutes * 60,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use warren_core::{InstanceStatus, InstanceType, Shape};

    const SMALL_MONTHLY: Money = 4_760_000;

    fn inst(status: InstanceStatus, meter_started_at: i64, metered_through: i64) -> Instance {
        Instance {
            status,
            shape: Shape {
                cpu: 2,
                memory: 4,
                disk: 4,
            },
            kind: InstanceType::Default,
            meter_started_at,
            metered_through,
            ..Default::default()
        }
    }

    #[test]
    fn sub_minute_spans_are_not_charged() {
        assert_eq!(advance(SMALL_MONTHLY, 0.0, 100, 159), None);
        assert_eq!(advance(SMALL_MONTHLY, 0.0, 100, 100), None);
        assert_eq!(advance(SMALL_MONTHLY, 0.0, 100, 40), None);
        assert_eq!(advance(SMALL_MONTHLY, 0.0, i64::MAX, i64::MIN), None);
    }

    #[test]
    fn one_minute_floors_and_carries() {
        // 4,760,000 / 43,800 = 108.675799...
        let s = advance(SMALL_MONTHLY, 0.0, 100, 160).unwrap();
        assert_eq!(s.minutes, 1);
        assert_eq!(s.charge_micros, 108);
        assert!(
            (s.carry_micros - 0.675_799_086_757_99).abs() < 1e-9,
            "{}",
            s.carry_micros
        );
        assert_eq!(s.metered_through, 160);
    }

    #[test]
    fn carry_rolls_into_the_next_charge() {
        let s = advance(SMALL_MONTHLY, 0.9, 100, 160).unwrap();
        assert_eq!(s.charge_micros, 109);
        assert!(
            (s.carry_micros - 0.575_799_086_757_99).abs() < 1e-9,
            "{}",
            s.carry_micros
        );
    }

    #[test]
    fn remainder_seconds_stay_unmetered() {
        let s = advance(SMALL_MONTHLY, 0.0, 100, 250).unwrap();
        assert_eq!(s.minutes, 2);
        assert_eq!(s.charge_micros, 217);
        assert_eq!(s.metered_through, 220);
    }

    #[test]
    fn corrupt_carry_and_negative_prices_are_tolerated() {
        assert_eq!(
            advance(SMALL_MONTHLY, f64::NAN, 0, 60)
                .unwrap()
                .charge_micros,
            108
        );
        assert_eq!(
            advance(SMALL_MONTHLY, f64::INFINITY, 0, 60)
                .unwrap()
                .charge_micros,
            108
        );
        assert_eq!(
            advance(SMALL_MONTHLY, -5.0, 0, 60).unwrap().charge_micros,
            108
        );
        let s = advance(-1, 0.0, 0, 60).unwrap();
        assert_eq!(s.charge_micros, 0);
        assert!(s.carry_micros.abs() < 1e-12);
        assert_eq!(s.metered_through, 60);
    }

    #[test]
    fn billing_rate_follows_status() {
        let rates = Rates::default();
        let compute = Some((LedgerKind::Compute, SMALL_MONTHLY));
        let disk = Some((LedgerKind::Disk, 360_000));
        for st in [
            InstanceStatus::Running,
            InstanceStatus::Stopping,
            InstanceStatus::Starting,
            InstanceStatus::Restarting,
            InstanceStatus::Updating,
        ] {
            assert_eq!(billing_rate(&rates, &inst(st, 1, 1)), compute, "{st}");
        }
        for st in [
            InstanceStatus::Sleeping,
            InstanceStatus::Stopped,
            InstanceStatus::Waking,
        ] {
            assert_eq!(billing_rate(&rates, &inst(st, 1, 1)), disk, "{st}");
        }
        for st in [
            InstanceStatus::Provisioning,
            InstanceStatus::Failed,
            InstanceStatus::Deleting,
            InstanceStatus::Deleted,
        ] {
            assert_eq!(billing_rate(&rates, &inst(st, 1, 1)), None, "{st}");
        }
        let mut vm = inst(InstanceStatus::Running, 1, 1);
        vm.kind = InstanceType::Vm;
        assert_eq!(
            billing_rate(&rates, &vm),
            Some((LedgerKind::Compute, 11_360_000))
        );
    }

    #[test]
    fn metered_from_prefers_metered_through() {
        assert_eq!(metered_from(&inst(InstanceStatus::Running, 0, 0)), None);
        assert_eq!(metered_from(&inst(InstanceStatus::Running, 0, 500)), None);
        assert_eq!(
            metered_from(&inst(InstanceStatus::Running, 100, 0)),
            Some(100)
        );
        assert_eq!(
            metered_from(&inst(InstanceStatus::Running, 100, 160)),
            Some(160)
        );
    }
}
