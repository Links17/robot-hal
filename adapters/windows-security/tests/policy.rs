use seeed_hal_windows_security::{NamedPipeDaclPolicy, TrustedPrincipal};

#[test]
fn named_pipe_policy_is_protected_and_has_only_three_trusted_principals() {
    let policy = NamedPipeDaclPolicy::current_user_private();

    assert!(policy.is_protected());
    assert_eq!(
        policy.trusted_principals(),
        &[
            TrustedPrincipal::CurrentUser,
            TrustedPrincipal::LocalSystem,
            TrustedPrincipal::Administrators,
        ],
    );
}

#[test]
fn generated_sddl_contains_no_inherited_or_broad_trustee() {
    let policy = NamedPipeDaclPolicy::current_user_private();
    let sddl = policy.sddl_for_current_user("S-1-5-21-1-2-3-1001");

    assert_eq!(
        sddl,
        "D:P(A;;GA;;;S-1-5-21-1-2-3-1001)(A;;GA;;;SY)(A;;GA;;;BA)",
    );
    assert!(!sddl.contains(";;;WD"));
    assert!(!sddl.contains(";;;AU"));
}
