//! Narrow Windows security policy and safe Named Pipe creation wrapper.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrustedPrincipal {
    CurrentUser,
    LocalSystem,
    Administrators,
}

const TRUSTED_PRINCIPALS: [TrustedPrincipal; 3] = [
    TrustedPrincipal::CurrentUser,
    TrustedPrincipal::LocalSystem,
    TrustedPrincipal::Administrators,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NamedPipeDaclPolicy;

impl NamedPipeDaclPolicy {
    pub const fn current_user_private() -> Self {
        Self
    }

    pub const fn is_protected(self) -> bool {
        true
    }

    pub const fn trusted_principals(self) -> &'static [TrustedPrincipal] {
        &TRUSTED_PRINCIPALS
    }

    pub fn sddl_for_current_user(self, current_user_sid: &str) -> String {
        format!("D:P(A;;GA;;;{current_user_sid})(A;;GA;;;SY)(A;;GA;;;BA)")
    }
}

#[cfg(windows)]
pub fn create_current_user_named_pipe(
    options: &tokio::net::windows::named_pipe::ServerOptions,
    name: impl AsRef<std::ffi::OsStr>,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use std::mem;

    use windows_permissions::{LocalBox, SecurityDescriptor};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    let current_user = windows_permissions::utilities::current_process_sid()?;
    let sddl = NamedPipeDaclPolicy::current_user_private()
        .sddl_for_current_user(&current_user.to_string());
    let descriptor = sddl.parse::<LocalBox<SecurityDescriptor>>()?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_ptr().cast(),
        bInheritHandle: 0,
    };

    // SAFETY: Tokio documents that `create_with_security_attributes_raw`
    // requires a valid `SECURITY_ATTRIBUTES` pointer. `attributes` and its
    // LocalAlloc-owned self-relative security descriptor remain alive for the
    // complete synchronous call, and CreateNamedPipeW does not retain either
    // pointer after returning. See Tokio 1.53.1 named_pipe.rs:2280-2304 and
    // https://learn.microsoft.com/windows/win32/api/namedpipeapi/nf-namedpipeapi-createnamedpipew.
    unsafe {
        options.create_with_security_attributes_raw(
            name,
            (&raw mut attributes).cast::<core::ffi::c_void>(),
        )
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::collections::BTreeSet;

    use windows_permissions::constants::SecurityInformation;
    use windows_permissions::{WindowsSecure, wrappers};

    use super::create_current_user_named_pipe;

    #[tokio::test]
    async fn created_pipe_has_protected_three_trustee_dacl() {
        let pipe_name = format!(r"\\.\pipe\robot-hal-dacl-test-{}", uuid::Uuid::new_v4());
        let mut options = tokio::net::windows::named_pipe::ServerOptions::new();
        options
            .reject_remote_clients(true)
            .first_pipe_instance(true);
        let server = create_current_user_named_pipe(&options, &pipe_name).unwrap();

        let descriptor = server
            .security_descriptor(SecurityInformation::Dacl)
            .unwrap();
        let sddl = wrappers::ConvertSecurityDescriptorToStringSecurityDescriptor(
            &descriptor,
            SecurityInformation::Dacl,
        )
        .unwrap()
        .to_string_lossy()
        .into_owned();
        assert!(sddl.starts_with("D:P"), "DACL must be protected: {sddl}");
        let dacl = descriptor.dacl().expect("pipe must have a DACL");
        assert_eq!(dacl.len(), 3);
        let trustees = (0..dacl.len())
            .map(|index| {
                dacl.get_ace(index)
                    .and_then(|ace| ace.sid())
                    .expect("every allowed ACE has a SID")
                    .to_string()
            })
            .collect::<BTreeSet<_>>();
        let expected = [
            windows_permissions::utilities::current_process_sid()
                .unwrap()
                .to_string(),
            "S-1-5-18".to_owned(),
            "S-1-5-32-544".to_owned(),
        ]
        .into_iter()
        .collect();
        assert_eq!(trustees, expected);
    }
}
