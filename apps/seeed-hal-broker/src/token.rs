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

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowsTokenShareModes {
    primary: u32,
    identity_reopen: u32,
}

#[cfg(any(windows, test))]
fn windows_token_share_modes() -> WindowsTokenShareModes {
    #[cfg(windows)]
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};
    #[cfg(not(windows))]
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    #[cfg(not(windows))]
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;

    WindowsTokenShareModes {
        primary: FILE_SHARE_READ,
        identity_reopen: FILE_SHARE_READ | FILE_SHARE_DELETE,
    }
}

#[cfg(test)]
mod share_mode_tests {
    use super::windows_token_share_modes;

    #[test]
    fn identity_reopen_shares_delete_without_weakening_the_primary_handle() {
        let share_modes = windows_token_share_modes();

        assert_eq!(share_modes.primary, 0x0000_0001);
        assert_eq!(share_modes.identity_reopen, 0x0000_0001 | 0x0000_0004);
    }
}

#[cfg(unix)]
mod platform {
    use std::ffi::OsString;
    use std::fs::File;
    use std::io::{self, Read};
    use std::path::{Path, PathBuf};

    use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};
    use zeroize::Zeroizing;

    use super::policy_error;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Identity {
        device: u64,
        inode: u64,
        owner: u32,
    }

    impl Identity {
        fn from(metadata: &Stat) -> Self {
            Self {
                device: metadata.st_dev as u64,
                inode: metadata.st_ino,
                owner: metadata.st_uid,
            }
        }
    }

    pub(super) struct TrustedTokenFile {
        name: OsString,
        file: File,
        file_identity: Identity,
        parent_file: File,
        parent_identity: Identity,
        effective_uid: u32,
    }

    impl TrustedTokenFile {
        pub(super) fn open(path: PathBuf) -> io::Result<Self> {
            let (parent, name) = parent_and_name(&path)?;
            let effective_uid = rustix::process::geteuid().as_raw();
            let parent_file = File::from(
                rustix::fs::open(
                    &parent,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(normalize_open_error)?,
            );
            let parent_metadata = rustix::fs::fstat(&parent_file).map_err(io::Error::from)?;
            validate_parent(&parent_metadata, effective_uid)?;
            let parent_identity = Identity::from(&parent_metadata);

            let path_metadata = rustix::fs::statat(&parent_file, &name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(io::Error::from)?;
            let file = File::from(
                rustix::fs::openat(
                    &parent_file,
                    &name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(normalize_open_error)?,
            );
            let metadata = rustix::fs::fstat(&file).map_err(io::Error::from)?;
            validate_file(&metadata, effective_uid)?;
            let file_identity = Identity::from(&metadata);
            if file_identity != Identity::from(&path_metadata) {
                return Err(policy_error("token path changed while it was being opened"));
            }
            Ok(Self {
                name,
                file,
                file_identity,
                parent_file,
                parent_identity,
                effective_uid,
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

            let parent_metadata = rustix::fs::fstat(&self.parent_file).map_err(io::Error::from)?;
            validate_parent(&parent_metadata, self.effective_uid)?;
            if Identity::from(&parent_metadata) != self.parent_identity {
                return Err(policy_error(
                    "token parent directory changed before deletion",
                ));
            }
            let metadata = rustix::fs::fstat(&self.file).map_err(io::Error::from)?;
            validate_file(&metadata, self.effective_uid)?;
            let path_metadata =
                rustix::fs::statat(&self.parent_file, &self.name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(io::Error::from)?;
            validate_file(&path_metadata, self.effective_uid)?;
            if Identity::from(&path_metadata) != self.file_identity {
                return Err(policy_error("token path changed before deletion"));
            }
            rustix::fs::unlinkat(&self.parent_file, &self.name, AtFlags::empty())
                .map_err(io::Error::from)?;
            Ok(token)
        }
    }

    fn parent_and_name(path: &Path) -> io::Result<(PathBuf, OsString)> {
        let name = path
            .file_name()
            .ok_or_else(|| policy_error("token path must name a file"))?
            .to_owned();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        Ok((parent.to_owned(), name))
    }

    fn validate_parent(metadata: &Stat, effective_uid: u32) -> io::Result<()> {
        if FileType::from_raw_mode(metadata.st_mode) != FileType::Directory {
            return Err(policy_error("token parent must be a real directory"));
        }
        if Mode::from_raw_mode(metadata.st_mode) & (Mode::RWXU | Mode::RWXG | Mode::RWXO)
            != Mode::RWXU
        {
            return Err(policy_error("token parent directory must have mode 0700"));
        }
        if metadata.st_uid != effective_uid {
            return Err(policy_error(
                "token parent directory must be owned by the effective user",
            ));
        }
        Ok(())
    }

    fn validate_file(metadata: &Stat, effective_uid: u32) -> io::Result<()> {
        if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
            return Err(policy_error("token path must name a regular file"));
        }
        let mode = Mode::from_raw_mode(metadata.st_mode);
        if !(mode.contains(Mode::RUSR)) || mode.intersects(Mode::RWXG | Mode::RWXO) {
            return Err(policy_error(
                "token file must be owner-readable and inaccessible to group/other",
            ));
        }
        if metadata.st_uid != effective_uid {
            return Err(policy_error(
                "token file must be owned by the effective user",
            ));
        }
        if metadata.st_nlink != 1 {
            return Err(policy_error("token file must have exactly one hard link"));
        }
        Ok(())
    }

    fn normalize_open_error(error: rustix::io::Errno) -> io::Error {
        if matches!(error, rustix::io::Errno::LOOP | rustix::io::Errno::MLINK) {
            policy_error("token file must not be a symbolic link")
        } else {
            error.into()
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::platform::TrustedTokenFile;

    static CWD_LOCK: Mutex<()> = Mutex::new(());

    struct RestoreCurrentDirectory(PathBuf);

    impl Drop for RestoreCurrentDirectory {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    fn private_directory(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "seeed-hal-token-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn write_token(path: &std::path::Path, byte: u8) {
        std::fs::write(path, [byte; 32]).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn bare_relative_token_uses_current_directory_as_parent() {
        let _cwd_lock = CWD_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let directory = private_directory("bare-relative");
        let path = directory.join("token");
        write_token(&path, 0x33);
        let restore = RestoreCurrentDirectory(std::env::current_dir().unwrap());
        std::env::set_current_dir(&directory).unwrap();
        let result = TrustedTokenFile::open(PathBuf::from("token")).and_then(|file| file.consume());
        drop(restore);

        assert_eq!(*result.unwrap(), [0x33; 32]);
        assert!(!path.exists());
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn hard_linked_token_is_rejected_without_deleting_either_link() {
        let directory = private_directory("hard-link");
        let path = directory.join("token");
        let link = directory.join("token-link");
        write_token(&path, 0x44);
        std::fs::hard_link(&path, &link).unwrap();

        assert_eq!(
            TrustedTokenFile::open(path.clone()).err().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(path.exists());
        assert!(link.exists());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(link).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn token_parent_and_file_must_be_owned_by_effective_user() {
        if !rustix::process::geteuid().is_root() {
            return;
        }
        let directory = private_directory("wrong-owner");
        let path = directory.join("token");
        write_token(&path, 0x55);
        let other = rustix::process::Uid::from_raw(1);
        rustix::fs::chown(&path, Some(other), None).unwrap();
        rustix::fs::chown(&directory, Some(other), None).unwrap();

        assert_eq!(
            TrustedTokenFile::open(path.clone()).err().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn pathname_replacement_is_rejected_without_deleting_replacement() {
        let directory = private_directory("replace");
        let path = directory.join("token");
        let original = directory.join("original");
        write_token(&path, 0x11);
        let trusted = TrustedTokenFile::open(path.clone()).unwrap();

        std::fs::rename(&path, &original).unwrap();
        write_token(&path, 0x22);

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

    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT,
    };

    use super::{policy_error, windows_token_share_modes};

    pub(super) struct TrustedTokenFile {
        path: PathBuf,
        file: File,
        identity: Identity,
        parent: PathBuf,
        parent_file: File,
        parent_identity: Identity,
        user: LocalBox<Sid>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Identity {
        volume_serial_number: u64,
        file_index: u64,
    }

    impl Identity {
        fn from(file: &File) -> io::Result<Self> {
            let information = winapi_util::file::information(file)?;
            Ok(Self {
                volume_serial_number: information.volume_serial_number(),
                file_index: information.file_index(),
            })
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
            let parent_identity = Identity::from(&parent_file)?;
            let user = current_process_sid()?;
            validate_handle_security(&parent_file, &user)?;

            let path_metadata = std::fs::symlink_metadata(&path)?;
            validate_real_file_path(&path_metadata)?;
            let share_modes = windows_token_share_modes();
            let file = OpenOptions::new()
                .read(true)
                .access_mode(GENERIC_READ | DELETE)
                .share_mode(share_modes.primary)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&path)?;
            let metadata = file.metadata()?;
            validate_real_file(&metadata, &file)?;
            let identity = Identity::from(&file)?;
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
            if Identity::from(&self.parent_file)? != self.parent_identity {
                return Err(policy_error("token parent changed before deletion"));
            }

            let metadata = self.file.metadata()?;
            validate_real_file(&metadata, &self.file)?;
            validate_handle_security(&self.file, &self.user)?;
            let path_metadata = std::fs::symlink_metadata(&self.path)?;
            validate_real_file_path(&path_metadata)?;
            let share_modes = windows_token_share_modes();
            // Windows checks sharing bidirectionally: the new handle's desired access must be
            // allowed by every existing handle's share mode, and every existing handle's access
            // must be allowed by the new handle's share mode. The primary handle owns DELETE
            // access, so this read-only identity re-open must share DELETE with it. The primary
            // handle itself still shares only reads, which continues to deny external DELETE-access
            // opens used for pathname replacement or deletion.
            let path_file = OpenOptions::new()
                .read(true)
                .share_mode(share_modes.identity_reopen)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&self.path)?;
            let path_file_metadata = path_file.metadata()?;
            validate_real_file(&path_file_metadata, &path_file)?;
            validate_handle_security(&path_file, &self.user)?;
            if Identity::from(&self.file)? != self.identity
                || Identity::from(&path_file)? != self.identity
            {
                return Err(policy_error("token path changed before deletion"));
            }
            drop(path_file);
            seeed_hal_windows_file::mark_delete_on_close(&self.file)?;
            drop(self.file);
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

    fn validate_real_file_path(metadata: &std::fs::Metadata) -> io::Result<()> {
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(policy_error(
                "token path must name a regular non-reparse file",
            ));
        }
        Ok(())
    }

    fn validate_real_file(metadata: &std::fs::Metadata, file: &File) -> io::Result<()> {
        validate_real_file_path(metadata)?;
        if winapi_util::file::information(file)?.number_of_links() != 1 {
            return Err(policy_error("token file must have exactly one hard link"));
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
        use std::os::windows::fs::OpenOptionsExt;
        use std::path::Path;

        use windows_permissions::constants::{SeObjectType, SecurityInformation};
        use windows_permissions::utilities::current_process_sid;
        use windows_permissions::{LocalBox, SecurityDescriptor};
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        use super::super::read_and_remove_token;
        use super::{Identity, validate_private_security_descriptor, validate_real_file};

        fn descriptor(sddl: &str) -> LocalBox<SecurityDescriptor> {
            sddl.parse().expect("test SDDL must be valid")
        }

        fn apply_private_acl(path: &Path) {
            let user = current_process_sid().unwrap();
            let descriptor = descriptor(&format!(
                "O:{user}D:P(A;;FA;;;{user})(A;;FA;;;SY)(A;;FA;;;BA)"
            ));
            windows_permissions::wrappers::SetNamedSecurityInfo(
                path.as_os_str(),
                SeObjectType::SE_FILE_OBJECT,
                SecurityInformation::Owner | SecurityInformation::Dacl,
                descriptor.owner(),
                None,
                descriptor.dacl(),
                None,
            )
            .unwrap();
        }

        #[tokio::test]
        async fn broker_app_reopens_and_deletes_private_token_by_validated_handle() {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "seeed-hal-windows-token-consume-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir(&directory).unwrap();
            apply_private_acl(&directory);
            let path = directory.join("token");
            std::fs::write(&path, [0x5a_u8; 32]).unwrap();
            apply_private_acl(&path);

            let token = read_and_remove_token(path.clone()).await.unwrap();

            assert_eq!(token.expose_bytes(), &[0x5a_u8; 32]);
            assert!(!path.exists());
            std::fs::remove_dir(directory).unwrap();
        }

        #[test]
        fn file_identity_is_handle_stable_and_hard_links_are_rejected() {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "seeed-hal-windows-token-{}-{nonce}",
                std::process::id()
            ));
            let link = path.with_extension("link");
            std::fs::write(&path, [0x66_u8; 32]).unwrap();
            let file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(&path)
                .unwrap();
            assert_eq!(
                Identity::from(&file).unwrap(),
                Identity::from(&file).unwrap()
            );
            std::fs::hard_link(&path, &link).unwrap();
            assert_eq!(
                validate_real_file(&file.metadata().unwrap(), &file)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
            drop(file);
            std::fs::remove_file(path).unwrap();
            std::fs::remove_file(link).unwrap();
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
