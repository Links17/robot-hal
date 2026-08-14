use std::io;
use std::path::PathBuf;

use seeed_hal_broker::StartupToken;

pub async fn read_and_remove_token(path: PathBuf) -> io::Result<StartupToken> {
    let token =
        tokio::task::spawn_blocking(move || platform::TrustedTokenFile::open(path)?.consume())
            .await
            .map_err(|error| io::Error::other(format!("token file task failed: {error}")))??;
    Ok(StartupToken::from_bytes(*token))
}

fn policy_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

#[cfg(unix)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::PathBuf;
    use zeroize::Zeroizing;

    use super::policy_error;

    #[derive(Clone, Copy, Eq, PartialEq)]
    struct Identity {
        device: u64,
        inode: u64,
        owner: u32,
    }

    impl Identity {
        fn from(metadata: &std::fs::Metadata) -> Self {
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                owner: metadata.uid(),
            }
        }
    }

    pub(super) struct TrustedTokenFile {
        path: PathBuf,
        file: File,
        file_identity: Identity,
        parent: PathBuf,
        parent_identity: Identity,
    }

    impl TrustedTokenFile {
        pub(super) fn open(path: PathBuf) -> io::Result<Self> {
            let parent = path
                .parent()
                .ok_or_else(|| policy_error("token file must have a private parent directory"))?
                .to_owned();
            let parent_metadata = std::fs::symlink_metadata(&parent)?;
            validate_parent(&parent_metadata)?;
            let parent_identity = Identity::from(&parent_metadata);

            let path_metadata = std::fs::symlink_metadata(&path)?;
            if path_metadata.file_type().is_symlink() {
                return Err(policy_error("token file must not be a symbolic link"));
            }
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)
                .map_err(normalize_open_error)?;
            let metadata = file.metadata()?;
            validate_file(&metadata, parent_identity.owner)?;
            let file_identity = Identity::from(&metadata);
            if file_identity != Identity::from(&path_metadata) {
                return Err(policy_error("token path changed while it was being opened"));
            }
            Ok(Self {
                path,
                file,
                file_identity,
                parent,
                parent_identity,
            })
        }

        pub(super) fn consume(mut self) -> io::Result<Zeroizing<[u8; 32]>> {
            let mut token = Zeroizing::new([0_u8; 32]);
            self.file.read_exact(&mut token[..])?;
            let mut extra = [0_u8; 1];
            if self.file.read(&mut extra)? != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "authentication token file must contain exactly 32 bytes",
                ));
            }

            let parent_metadata = std::fs::symlink_metadata(&self.parent)?;
            validate_parent(&parent_metadata)?;
            if Identity::from(&parent_metadata) != self.parent_identity {
                return Err(policy_error(
                    "token parent directory changed before deletion",
                ));
            }
            let path_metadata = std::fs::symlink_metadata(&self.path)?;
            validate_file(&path_metadata, self.parent_identity.owner)?;
            if Identity::from(&path_metadata) != self.file_identity {
                return Err(policy_error("token path changed before deletion"));
            }
            std::fs::remove_file(&self.path)?;
            Ok(token)
        }
    }

    fn validate_parent(metadata: &std::fs::Metadata) -> io::Result<()> {
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(policy_error("token parent must be a real directory"));
        }
        if metadata.permissions().mode() & 0o777 != 0o700 {
            return Err(policy_error("token parent directory must have mode 0700"));
        }
        Ok(())
    }

    fn validate_file(metadata: &std::fs::Metadata, parent_owner: u32) -> io::Result<()> {
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(policy_error("token path must name a regular file"));
        }
        if metadata.permissions().mode() & 0o077 != 0 || metadata.permissions().mode() & 0o400 == 0
        {
            return Err(policy_error(
                "token file must be owner-readable and inaccessible to group/other",
            ));
        }
        if metadata.uid() != parent_owner {
            return Err(policy_error(
                "token file and private parent must have the same owner",
            ));
        }
        if metadata.nlink() != 1 {
            return Err(policy_error("token file must have exactly one hard link"));
        }
        Ok(())
    }

    fn normalize_open_error(error: io::Error) -> io::Error {
        if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::EMLINK)) {
            policy_error("token file must not be a symbolic link")
        } else {
            error
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::platform::TrustedTokenFile;

    #[test]
    fn pathname_replacement_is_rejected_without_deleting_replacement() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "seeed-hal-token-replace-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("token");
        let original = directory.join("original");
        std::fs::write(&path, [0x11_u8; 32]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let trusted = TrustedTokenFile::open(path.clone()).unwrap();

        std::fs::rename(&path, &original).unwrap();
        std::fs::write(&path, [0x22_u8; 32]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(
            trusted.consume().unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(std::fs::read(&path).unwrap(), [0x22_u8; 32]);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(original).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read};
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::path::PathBuf;
    use windows_permissions::constants::{AceFlags, AceType, SeObjectType, SecurityInformation};
    use windows_permissions::utilities::current_process_sid;
    use windows_permissions::{LocalBox, SecurityDescriptor, Sid, Trustee};
    use zeroize::Zeroizing;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    use super::policy_error;

    pub(super) struct TrustedTokenFile {
        path: PathBuf,
        file: File,
        identity: Identity,
        parent: PathBuf,
        parent_file: File,
        parent_identity: Identity,
        user: LocalBox<Sid>,
    }

    #[derive(Clone, Copy, Eq, PartialEq)]
    struct Identity {
        attributes: u32,
        created: u64,
        modified: u64,
        size: u64,
    }

    impl Identity {
        fn from(metadata: &std::fs::Metadata) -> Self {
            Self {
                attributes: metadata.file_attributes(),
                created: metadata.creation_time(),
                modified: metadata.last_write_time(),
                size: metadata.file_size(),
            }
        }
    }

    impl TrustedTokenFile {
        pub(super) fn open(path: PathBuf) -> io::Result<Self> {
            let parent = path
                .parent()
                .ok_or_else(|| policy_error("token file must have a private parent directory"))?
                .to_owned();
            let parent_path_metadata = std::fs::symlink_metadata(&parent)?;
            validate_real_directory(&parent_path_metadata)?;
            let parent_file = OpenOptions::new()
                .read(true)
                .share_mode(0)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
                .open(&parent)?;
            let parent_metadata = parent_file.metadata()?;
            validate_real_directory(&parent_metadata)?;
            let parent_identity = Identity::from(&parent_metadata);
            if parent_identity != Identity::from(&parent_path_metadata) {
                return Err(policy_error(
                    "token parent path changed while it was being opened",
                ));
            }
            let user = current_process_sid()?;
            validate_handle_security(&parent_file, &user)?;

            let path_metadata = std::fs::symlink_metadata(&path)?;
            validate_real_file(&path_metadata)?;
            let file = OpenOptions::new()
                .read(true)
                .share_mode(0)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&path)?;
            let metadata = file.metadata()?;
            validate_real_file(&metadata)?;
            let identity = Identity::from(&metadata);
            if identity != Identity::from(&path_metadata) {
                return Err(policy_error("token path changed while it was being opened"));
            }
            validate_handle_security(&file, &user)?;
            Ok(Self {
                path,
                file,
                identity,
                parent,
                parent_file,
                parent_identity,
                user,
            })
        }

        pub(super) fn consume(mut self) -> io::Result<Zeroizing<[u8; 32]>> {
            let mut token = Zeroizing::new([0_u8; 32]);
            self.file.read_exact(&mut token[..])?;
            let mut extra = [0_u8; 1];
            if self.file.read(&mut extra)? != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "authentication token file must contain exactly 32 bytes",
                ));
            }

            let parent_metadata = self.parent_file.metadata()?;
            validate_real_directory(&parent_metadata)?;
            validate_handle_security(&self.parent_file, &self.user)?;
            let parent_path_metadata = std::fs::symlink_metadata(&self.parent)?;
            validate_real_directory(&parent_path_metadata)?;
            if Identity::from(&parent_metadata) != self.parent_identity
                || Identity::from(&parent_path_metadata) != self.parent_identity
            {
                return Err(policy_error("token parent changed before deletion"));
            }

            let metadata = self.file.metadata()?;
            validate_real_file(&metadata)?;
            validate_handle_security(&self.file, &self.user)?;
            let path_metadata = std::fs::symlink_metadata(&self.path)?;
            validate_real_file(&path_metadata)?;
            if Identity::from(&metadata) != self.identity
                || Identity::from(&path_metadata) != self.identity
            {
                return Err(policy_error("token path changed before deletion"));
            }
            drop(self.file);
            std::fs::remove_file(&self.path)?;
            Ok(token)
        }
    }

    fn validate_real_directory(metadata: &std::fs::Metadata) -> io::Result<()> {
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(policy_error(
                "token parent must be a non-reparse private directory",
            ));
        }
        Ok(())
    }

    fn validate_real_file(metadata: &std::fs::Metadata) -> io::Result<()> {
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(policy_error(
                "token path must name a regular non-reparse file",
            ));
        }
        Ok(())
    }

    fn validate_handle_security(file: &File, user: &Sid) -> io::Result<()> {
        let descriptor = windows_permissions::wrappers::GetSecurityInfo(
            file,
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Owner | SecurityInformation::Dacl,
        )?;
        validate_private_security_descriptor(&descriptor, user)
    }

    fn validate_private_security_descriptor(
        descriptor: &SecurityDescriptor,
        user: &Sid,
    ) -> io::Result<()> {
        if descriptor.owner() != Some(user) {
            return Err(policy_error(
                "token file and parent must be owned by the current user",
            ));
        }
        let dacl = descriptor
            .dacl()
            .ok_or_else(|| policy_error("token file and parent must have a private DACL"))?;
        let system: LocalBox<Sid> = "SY".parse()?;
        let administrators: LocalBox<Sid> = "BA".parse()?;
        let owner_rights: LocalBox<Sid> = "OW".parse()?;

        for index in 0..dacl.len() {
            let ace = dacl
                .get_ace(index)
                .ok_or_else(|| policy_error("token DACL changed while it was inspected"))?;
            if !matches!(
                ace.ace_type(),
                AceType::ACCESS_ALLOWED_ACE_TYPE
                    | AceType::ACCESS_ALLOWED_CALLBACK_ACE_TYPE
                    | AceType::ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE
                    | AceType::ACCESS_ALLOWED_OBJECT_ACE_TYPE
            ) || ace.flags().contains(AceFlags::InheritOnly)
            {
                continue;
            }
            let sid = ace
                .sid()
                .ok_or_else(|| policy_error("token DACL contains an invalid access entry"))?;
            if sid == user
                || sid == system.as_ref()
                || sid == administrators.as_ref()
                || sid == owner_rights.as_ref()
            {
                continue;
            }
            let trustee: Trustee<'_> = sid.into();
            if !dacl.effective_rights(&trustee)?.is_empty() {
                return Err(policy_error(
                    "token file and parent grant access to an untrusted principal",
                ));
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use std::io;

        use windows_permissions::utilities::current_process_sid;
        use windows_permissions::{LocalBox, SecurityDescriptor};

        use super::validate_private_security_descriptor;

        fn descriptor(sddl: &str) -> LocalBox<SecurityDescriptor> {
            sddl.parse().expect("test SDDL must be valid")
        }

        #[test]
        fn security_descriptor_rejects_owner_other_than_current_user() {
            let user = current_process_sid().unwrap();
            let descriptor = descriptor(&format!("O:SYD:P(A;;FA;;;{user})"));

            assert_eq!(
                validate_private_security_descriptor(&descriptor, &user)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }

        #[test]
        fn security_descriptor_rejects_missing_dacl() {
            let user = current_process_sid().unwrap();
            let descriptor = descriptor(&format!("O:{user}"));

            assert_eq!(
                validate_private_security_descriptor(&descriptor, &user)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }

        #[test]
        fn security_descriptor_rejects_access_for_untrusted_principal() {
            let user = current_process_sid().unwrap();
            let descriptor = descriptor(&format!("O:{user}D:P(A;;FA;;;{user})(A;;FR;;;WD)"));

            assert_eq!(
                validate_private_security_descriptor(&descriptor, &user)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }

        #[test]
        fn security_descriptor_allows_user_system_and_administrators() {
            let user = current_process_sid().unwrap();
            let descriptor = descriptor(&format!(
                "O:{user}D:P(A;;FA;;;{user})(A;;FA;;;SY)(A;;FA;;;BA)"
            ));

            validate_private_security_descriptor(&descriptor, &user).unwrap();
        }
    }
}
