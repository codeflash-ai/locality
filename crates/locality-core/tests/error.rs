use locality_core::LocalityError;

#[test]
fn update_required_error_has_exact_display() {
    let error = LocalityError::UpdateRequired {
        component: "linear:discovery".to_string(),
        found: 2,
        supported: 1,
    };

    assert_eq!(
        error.to_string(),
        "update required for linear:discovery: found version 2, supported version 1"
    );
}
