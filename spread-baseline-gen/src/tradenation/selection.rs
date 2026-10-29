use std::collections::BTreeSet;

use color_eyre::{Result, eyre::ensure};

use crate::universe::WorkItem;

pub(super) fn requested(items: &[WorkItem], only: &[String]) -> Result<BTreeSet<String>> {
    let available = items
        .iter()
        .map(|item| item.symbol.clone())
        .collect::<BTreeSet<_>>();
    let selected = if only.is_empty() {
        available.clone()
    } else {
        only.iter().cloned().collect()
    };
    let unknown = selected.difference(&available).collect::<Vec<_>>();
    ensure!(
        unknown.is_empty(),
        "Unknown TradeNation markets: {unknown:?}"
    );
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_stocks_do_not_shrink_the_checkpoint_universe() {
        super::super::tests::init();
        let items = super::super::universe().unwrap();
        let selected = requested(&items, &["LKQ".into(), "LUMEN TECH".into()]).unwrap();
        assert_eq!(
            selected.into_iter().collect::<Vec<_>>(),
            ["LKQ", "LUMEN TECH"]
        );
        assert!(items.len() > 1000);
    }

    #[test]
    fn unknown_names_fail_before_any_checkpoint_is_changed() {
        super::super::tests::init();
        let items = super::super::universe().unwrap();
        assert!(requested(&items, &["UNKNOWN STOCK".into()]).is_err());
    }

    #[test]
    fn empty_selection_measures_the_whole_universe() {
        super::super::tests::init();
        let items = super::super::universe().unwrap();
        assert_eq!(requested(&items, &[]).unwrap().len(), items.len());
    }
}
