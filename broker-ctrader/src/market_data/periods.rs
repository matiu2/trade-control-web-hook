//! Exact ProtoOATrendbarPeriod values; unsupported periods must be aggregated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePeriod {
    Minute1,
    Minute2,
    Minute3,
    Minute4,
    Minute5,
    Minute10,
    Minute15,
    Minute30,
    Hour1,
    Hour4,
    Hour12,
    Day1,
    Week1,
    Month1,
}

impl NativePeriod {
    pub(crate) fn wire(self) -> (i64, &'static str) {
        match self {
            Self::Minute1 => (1, "M1"),
            Self::Minute2 => (2, "M2"),
            Self::Minute3 => (3, "M3"),
            Self::Minute4 => (4, "M4"),
            Self::Minute5 => (5, "M5"),
            Self::Minute10 => (6, "M10"),
            Self::Minute15 => (7, "M15"),
            Self::Minute30 => (8, "M30"),
            Self::Hour1 => (9, "H1"),
            Self::Hour4 => (10, "H4"),
            Self::Hour12 => (11, "H12"),
            Self::Day1 => (12, "D1"),
            Self::Week1 => (13, "W1"),
            Self::Month1 => (14, "MN1"),
        }
    }

    /// Minimum native duration, used only to bound request size, not alignment.
    pub(crate) fn minimum_minutes(self) -> i64 {
        match self {
            Self::Minute1 => 1,
            Self::Minute2 => 2,
            Self::Minute3 => 3,
            Self::Minute4 => 4,
            Self::Minute5 => 5,
            Self::Minute10 => 10,
            Self::Minute15 => 15,
            Self::Minute30 => 30,
            Self::Hour1 => 60,
            Self::Hour4 => 240,
            Self::Hour12 => 720,
            Self::Day1 => 1440,
            Self::Week1 => 10080,
            Self::Month1 => 40320,
        }
    }
}
