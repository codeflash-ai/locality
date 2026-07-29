use locality_core::workspace_layout::{
    MountTarget, MountTargetError, PortableMountId, PortableMountIdError,
    WORKSPACE_LAYOUT_UNICODE_VERSION,
};

#[test]
fn workspace_layout_unicode_data_is_frozen_at_16() {
    assert_eq!(WORKSPACE_LAYOUT_UNICODE_VERSION, (16, 0, 0));
    assert_eq!(unicode_normalization_v16::UNICODE_VERSION, (16, 0, 0));
    assert_eq!(caseless::UNICODE_VERSION, (16, 0, 0));
}

#[test]
fn portable_mount_ids_are_distinct_validated_opaque_values() {
    let id = PortableMountId::new("provider/workspace\\opaque").expect("opaque ID");
    assert_eq!(id.as_str(), "provider/workspace\\opaque");
    assert_eq!(
        serde_json::to_string(&id).expect("serialize"),
        r#""provider/workspace\\opaque""#
    );

    assert_eq!(PortableMountId::new(""), Err(PortableMountIdError::Empty));
    assert_eq!(
        PortableMountId::new("e\u{301}"),
        Err(PortableMountIdError::NotNfc)
    );
    assert!(matches!(
        PortableMountId::new("x".repeat(129)),
        Err(PortableMountIdError::TooManyUtf8Bytes { actual: 129 })
    ));
    assert_eq!(
        PortableMountId::new("opaque\0id"),
        Err(PortableMountIdError::Nul)
    );
    assert!(matches!(
        PortableMountId::new("opaque\u{7f}id"),
        Err(PortableMountIdError::Control('\u{7f}'))
    ));
    assert!(serde_json::from_str::<PortableMountId>(r#""e\u0301""#).is_err());
}

#[test]
fn mount_targets_enforce_portable_component_rules() {
    for target in ["Engineering", "Straße", "é", "has internal spaces"] {
        assert_eq!(
            MountTarget::new(target).expect("valid target").as_str(),
            target
        );
    }
    assert!(MountTarget::new("x".repeat(120)).is_ok());
    assert!(matches!(
        MountTarget::new("x".repeat(121)),
        Err(MountTargetError::TooManyUtf8Bytes { actual: 121 })
    ));
    assert_eq!(MountTarget::new(""), Err(MountTargetError::Empty));
    assert_eq!(MountTarget::new("."), Err(MountTargetError::Traversal));
    assert_eq!(MountTarget::new(".."), Err(MountTargetError::Traversal));
    assert_eq!(MountTarget::new("e\u{301}"), Err(MountTargetError::NotNfc));
    assert_eq!(
        MountTarget::new("trailing."),
        Err(MountTargetError::TrailingDotOrSpace)
    );
    assert_eq!(
        MountTarget::new("trailing "),
        Err(MountTargetError::TrailingDotOrSpace)
    );

    for character in ['/', '\\', ':', '<', '>', '"', '|', '?', '*'] {
        let value = format!("bad{character}name");
        assert_eq!(
            MountTarget::new(value),
            Err(MountTargetError::InvalidCharacter(character))
        );
    }
    assert_eq!(MountTarget::new("bad\0name"), Err(MountTargetError::Nul));
    assert!(matches!(
        MountTarget::new("bad\u{7f}name"),
        Err(MountTargetError::Control('\u{7f}'))
    ));
}

#[test]
fn mount_targets_reject_devices_and_unicode_folded_control_name() {
    for target in [
        "CON", "con.txt", "PrN", "AUX.log", "nul", "COM1", "com9.ext", "LPT1", "lpt9.txt",
    ] {
        assert_eq!(
            MountTarget::new(target),
            Err(MountTargetError::WindowsDeviceName),
            "accepted {target}"
        );
    }
    for target in ["COM0", "COM10", "LPT0", "LPT10", "CONSOLE"] {
        assert!(MountTarget::new(target).is_ok(), "rejected {target}");
    }
    for target in [".loc", ".LOC"] {
        assert_eq!(
            MountTarget::new(target),
            Err(MountTargetError::ReservedControlComponent)
        );
    }
}

#[test]
fn collision_key_is_full_default_non_turkic_fold_then_nfc() {
    let sharp_s = MountTarget::new("Straße").expect("target");
    let expanded = MountTarget::new("STRASSE").expect("target");
    assert_eq!(sharp_s.collision_key(), "strasse");
    assert_eq!(sharp_s.collision_key(), expanded.collision_key());

    let dotted_i = MountTarget::new("İ").expect("target");
    assert_eq!(dotted_i.collision_key(), "i\u{307}");
}

#[test]
fn collision_key_folds_greek_final_sigma() {
    let capital = MountTarget::new("ΟΣ").expect("target");
    let medial = MountTarget::new("οσ").expect("target");
    let final_sigma = MountTarget::new("ος").expect("target");

    assert_eq!(capital.collision_key(), "οσ");
    assert_eq!(capital.collision_key(), medial.collision_key());
    assert_eq!(capital.collision_key(), final_sigma.collision_key());
}

#[test]
fn collision_key_normalizes_after_full_default_fold() {
    let precomposed = MountTarget::new("SŚ").expect("NFC target");
    let fold_then_compose = MountTarget::new("ß\u{301}").expect("NFC target");

    assert_eq!(precomposed.collision_key(), "sś");
    assert_eq!(
        precomposed.collision_key(),
        fold_then_compose.collision_key()
    );
}
