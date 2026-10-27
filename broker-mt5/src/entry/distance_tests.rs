use super::*;
use mt5_data_source::web::trading::Rules;
use trade_control_core::intent::RiskBudget;

fn symbol() -> SymbolInfo {
    SymbolInfo {
        name: "AUDJPY".into(),
        digits: 3,
        point: 0.001,
        tick_size: 0.0,
        contract_size: 100_000.0,
        calc_mode: 0,
        base_currency: "AUD".into(),
        profit_currency: "JPY".into(),
        rules: Rules {
            stops_points: 10,
            ..crate::tests::rules()
        },
    }
}

fn request(entry: ResolvedEntry, direction: Direction) -> EntryRequest<'static> {
    EntryRequest {
        setup_id: None,
        instrument: "AUDJPY",
        direction,
        entry,
        stop_loss: 109.720,
        take_profit: 110.999,
        risk: RiskBudget::Percent(1.0),
        dry_run: true,
        contract_multiplier: None,
    }
}

#[test]
fn recorded_audjpy_prices_provide_specific_stop_and_limit_distance_evidence() {
    crate::tests::logging();
    [
        ResolvedEntry::Stop {
            trigger_price: 110.138,
        },
        ResolvedEntry::Limit {
            trigger_price: 110.138,
        },
    ]
    .into_iter()
    .for_each(|entry| {
        let req = request(entry, Direction::Long);
        let error = entry_price(&req, &symbol(), 110.123, 110.133).unwrap_err();
        let EntryError::EntryWithinMinimumDistance(distance) = error else {
            panic!("{error:?}");
        };
        assert!(distance.is_near());
        let message = distance.to_string();
        ["110.138", "110.123", "110.133", "0.01", "before submission"]
            .into_iter()
            .for_each(|part| assert!(message.contains(part), "{message}"));
    });
}

#[test]
fn short_near_prices_are_specific_but_far_wrong_side_errors_are_not() {
    crate::tests::logging();
    let req = request(
        ResolvedEntry::Stop {
            trigger_price: 110.138,
        },
        Direction::Short,
    );
    assert!(matches!(
        entry_price(&req, &symbol(), 110.143, 110.147),
        Err(EntryError::EntryWithinMinimumDistance(_))
    ));
    assert!(matches!(
        entry_price(&req, &symbol(), 109.500, 109.504),
        Err(EntryError::EntryTooCloseToMarket)
    ));
}

#[test]
fn exact_minimum_distance_is_accepted_and_no_minimum_never_proves_proximity() {
    crate::tests::logging();
    let req = request(
        ResolvedEntry::Stop {
            trigger_price: 110.138,
        },
        Direction::Long,
    );
    assert!(entry_price(&req, &symbol(), 110.124, 110.128).is_ok());
    let s = SymbolInfo {
        rules: Rules {
            stops_points: 0,
            ..crate::tests::rules()
        },
        ..symbol()
    };
    assert!(matches!(
        entry_price(&req, &s, 110.138, 110.142),
        Err(EntryError::EntryTooCloseToMarket)
    ));
}
