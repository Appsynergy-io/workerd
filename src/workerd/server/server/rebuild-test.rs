use std::time::Duration;

use super::Debt;
use super::REBUILD_DEBT_ALLOWANCE;
use super::charge_group;
use super::group_past_allowance;

#[test]
fn the_services_of_one_group_share_its_debt() {
    let now = Duration::from_secs(1000);
    let past = REBUILD_DEBT_ALLOWANCE + Duration::from_secs(1);
    assert!(!group_past_allowance(Some("shared"), now));
    charge_group(Some("shared"), past, now);
    // Any service naming the group is past it now, whatever its name.
    assert!(group_past_allowance(Some("shared"), now));
    // Another group's debt is its own.
    assert!(!group_past_allowance(Some("other"), now));
    // Wall time repays the group's debt as it does a service's.
    assert!(!group_past_allowance(
        Some("shared"),
        now + Duration::from_secs(1)
    ));
}

#[test]
fn a_service_in_no_group_owes_no_group_debt() {
    let now = Duration::from_secs(1000);
    charge_group(None, REBUILD_DEBT_ALLOWANCE * 10, now);
    assert!(!group_past_allowance(None, now));
}

#[test]
fn wall_time_repays_the_debt() {
    let mut debt = Debt::default();
    debt.charge(
        REBUILD_DEBT_ALLOWANCE + Duration::from_secs(1),
        Duration::ZERO,
    );
    assert!(debt.past_allowance(Duration::ZERO));
    assert!(debt.past_allowance(Duration::from_millis(999)));
    assert!(!debt.past_allowance(Duration::from_secs(1)));
    // Repaid debt never goes below nothing.
    assert!(!debt.past_allowance(Duration::from_secs(100)));
    debt.charge(Duration::from_secs(1), Duration::from_secs(100));
    assert_eq!(debt.debt, Duration::from_secs(1));
}
