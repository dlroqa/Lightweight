//! Byte quantities.
//!
//! The RAM estimator adds weights to KV cache to compute buffers to runtime
//! overhead, and compares the total against what the OS reports as available.
//! Those numbers pass through several crates, so they get a newtype: a `u64`
//! that is silently a count of tokens rather than bytes is exactly the kind of
//! bug that produces a confident, wrong "SAFE" verdict.

use std::fmt;
use std::ops::{Add, AddAssign, Sub};

use serde::{Deserialize, Serialize};

/// A quantity of bytes.
///
/// Serializes as a plain integer so API payloads stay conventional; the
/// human-readable form is produced by `Display` for UI and log use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Bytes(pub u64);

impl Bytes {
    pub const ZERO: Self = Self(0);
    pub const KIB: u64 = 1024;
    pub const MIB: u64 = 1024 * Self::KIB;
    pub const GIB: u64 = 1024 * Self::MIB;

    pub const fn from_kib(kib: u64) -> Self {
        Self(kib.saturating_mul(Self::KIB))
    }

    pub const fn from_mib(mib: u64) -> Self {
        Self(mib.saturating_mul(Self::MIB))
    }

    pub const fn from_gib(gib: u64) -> Self {
        Self(gib.saturating_mul(Self::GIB))
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// Difference, floored at zero.
    ///
    /// Memory arithmetic is full of subtractions that can legitimately go
    /// negative — "budget minus what we need" when we need more than the
    /// budget. Wrapping to 18 exabytes there would turn an INSUFFICIENT verdict
    /// into a wildly SAFE one, so saturation is the only defensible default.
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    /// `self` as a fraction of `total`. Returns 0.0 when `total` is zero.
    pub fn fraction_of(self, total: Self) -> f64 {
        if total.0 == 0 {
            0.0
        } else {
            self.0 as f64 / total.0 as f64
        }
    }
}

impl fmt::Display for Bytes {
    /// Binary units, matching how RAM is universally described. Two decimal
    /// places below 10 units, one above, so "3.2 / 16 GB" style readouts stay
    /// stable in width as values change.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(u64, &str); 4] = [
            (Bytes::GIB, "GiB"),
            (Bytes::MIB, "MiB"),
            (Bytes::KIB, "KiB"),
            (1, "B"),
        ];

        for (scale, suffix) in UNITS {
            if self.0 >= scale {
                if scale == 1 {
                    return write!(f, "{} {suffix}", self.0);
                }
                let value = self.0 as f64 / scale as f64;
                let precision = if value < 10.0 { 2 } else { 1 };
                return write!(f, "{value:.precision$} {suffix}");
            }
        }
        write!(f, "0 B")
    }
}

impl Add for Bytes {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        self.saturating_add(rhs)
    }
}

impl AddAssign for Bytes {
    fn add_assign(&mut self, rhs: Self) {
        *self = self.saturating_add(rhs);
    }
}

impl Sub for Bytes {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        self.saturating_sub(rhs)
    }
}

impl std::iter::Sum for Bytes {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, Add::add)
    }
}

impl From<u64> for Bytes {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_in_binary_units() {
        assert_eq!(Bytes(0).to_string(), "0 B");
        assert_eq!(Bytes(512).to_string(), "512 B");
        assert_eq!(Bytes::from_kib(4).to_string(), "4.00 KiB");
        assert_eq!(Bytes::from_mib(768).to_string(), "768.0 MiB");
        assert_eq!(Bytes::from_gib(2).to_string(), "2.00 GiB");
    }

    #[test]
    fn subtraction_saturates_instead_of_wrapping() {
        // "budget - required" when required exceeds the budget. Wrapping here
        // would report ~18 exabytes of headroom and pass an INSUFFICIENT load
        // as SAFE.
        let budget = Bytes::from_gib(2);
        let required = Bytes::from_gib(6);
        assert_eq!(budget - required, Bytes::ZERO);
    }

    #[test]
    fn addition_saturates_at_the_ceiling() {
        assert_eq!(Bytes(u64::MAX) + Bytes(1), Bytes(u64::MAX));
    }

    #[test]
    fn sums_over_an_iterator() {
        let total: Bytes = [Bytes::from_mib(100), Bytes::from_mib(150)]
            .into_iter()
            .sum();
        assert_eq!(total, Bytes::from_mib(250));
    }

    #[test]
    fn fraction_of_zero_is_zero_not_nan() {
        // A NaN here would propagate into a percentage on the dashboard.
        assert_eq!(Bytes(100).fraction_of(Bytes::ZERO), 0.0);
    }

    #[test]
    fn serializes_as_a_plain_integer() {
        let json = serde_json::to_string(&Bytes::from_mib(1)).expect("serialize");
        assert_eq!(json, "1048576");
    }
}
