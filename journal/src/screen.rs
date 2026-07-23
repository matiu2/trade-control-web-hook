//! The left→right screen stack. `→` pushes deeper, `←` pops back toward the
//! list. Each plan remembers the deepest screen reached so the delete key can
//! gate on "you've looked at it at least once" (depth ≥ 1).

/// One screen in the stack, ordered by depth. `List` is depth 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Screen {
    /// The plan picker.
    List,
    /// Timeline of recorded events; on first entry, also fills the info bar
    /// (from `plan export`) and auto-loads TradingView.
    Timeline,
    /// The replay report (`replay-candles --plan`).
    Replay,
    /// Replay report ‖ live timeline (v1); computed divergence diff (v2).
    Compare,
}

impl Screen {
    /// Depth of this screen (0 = list). Used for the delete guard.
    pub fn depth(self) -> u8 {
        match self {
            Screen::List => 0,
            Screen::Timeline => 1,
            Screen::Replay => 2,
            Screen::Compare => 3,
        }
    }

    /// The next screen deeper, or `None` at the deepest (`Compare`).
    pub fn deeper(self) -> Option<Screen> {
        match self {
            Screen::List => Some(Screen::Timeline),
            Screen::Timeline => Some(Screen::Replay),
            Screen::Replay => Some(Screen::Compare),
            Screen::Compare => None,
        }
    }

    /// The previous screen shallower, or `None` at the list (depth 0).
    pub fn shallower(self) -> Option<Screen> {
        match self {
            Screen::List => None,
            Screen::Timeline => Some(Screen::List),
            Screen::Replay => Some(Screen::Timeline),
            Screen::Compare => Some(Screen::Replay),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_ordering() {
        assert_eq!(Screen::List.depth(), 0);
        assert!(Screen::Timeline.depth() >= 1);
        assert!(Screen::Compare > Screen::Replay);
    }

    #[test]
    fn deeper_shallower_are_inverse() {
        for s in [
            Screen::List,
            Screen::Timeline,
            Screen::Replay,
            Screen::Compare,
        ] {
            if let Some(d) = s.deeper() {
                assert_eq!(d.shallower(), Some(s));
            }
        }
    }

    #[test]
    fn compare_is_deepest() {
        assert_eq!(Screen::Compare.deeper(), None);
        assert_eq!(Screen::List.shallower(), None);
    }
}
