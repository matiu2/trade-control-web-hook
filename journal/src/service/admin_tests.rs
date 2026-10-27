use super::*;
use crate::plan_admin::PlanAction;
use trade_control_core::{incoming::parse_and_verify, intent::Action};

#[test]
fn administration_signs_each_action_and_explicit_detachment() {
    [
        (
            PlanAction::Paper {
                account: "next".into(),
            },
            Action::PlanPaper,
            "plan-paper: selected; changed",
        ),
        (
            PlanAction::SwapAccount {
                account: "next".into(),
            },
            Action::PlanSwapAccount,
            "plan-swapped: selected old -> next",
        ),
        (
            PlanAction::ClosedManually,
            Action::PlanClosedManually,
            "plan-closed-manually: selected account=old",
        ),
    ]
    .into_iter()
    .for_each(|(action, expected, response)| {
        let (client, worker) = super::deletion_tests::mock_worker("200 OK", response);
        assert_eq!(client.administer("selected", &action).unwrap(), response);
        let body = worker.join().unwrap();
        let verified = parse_and_verify(&body, &[7; KEY_LEN], Utc::now()).unwrap();
        assert_eq!(verified.intent.action, expected);
        assert_eq!(verified.intent.trade_id.as_deref(), Some("selected"));
        let options = verified.intent.plan_admin.unwrap();
        assert!(options.detach_existing);
        assert_eq!(
            options.target_account.as_deref(),
            if expected == Action::PlanClosedManually {
                None
            } else {
                Some("next")
            }
        );
    });
}

#[test]
fn administration_rejects_bad_acknowledgement_and_worker_refusal() {
    [
        ("200 OK", "plan-paper: other; changed"),
        ("200 OK", ""),
        ("409 Conflict", "ambiguous trade"),
    ]
    .into_iter()
    .for_each(|(status, response)| {
        let (client, worker) = super::deletion_tests::mock_worker(status, response);
        assert!(
            client
                .administer(
                    "selected",
                    &PlanAction::Paper {
                        account: "next".into()
                    }
                )
                .is_err()
        );
        worker.join().unwrap();
    });
}
