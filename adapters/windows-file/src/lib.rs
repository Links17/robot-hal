//! Narrow safe wrappers for Windows file-handle operations missing from the
//! Rust 1.85 standard library.

#[cfg(windows)]
pub fn mark_delete_on_close(file: &std::fs::File) -> std::io::Result<()> {
    use std::mem;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    };

    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: `file` owns a valid handle for the duration of this synchronous
    // call. `disposition` is initialized, correctly sized, and the Windows API
    // does not retain its pointer after returning.
    let result = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfo,
            (&raw const disposition).cast(),
            mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_SHARE_READ};

    use super::mark_delete_on_close;

    #[test]
    fn deletion_is_bound_to_the_validated_handle_when_path_replacement_is_attempted() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "robot-hal-handle-delete-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("token");
        let moved = directory.join("moved");
        let replacement = directory.join("replacement");
        std::fs::write(&path, [0x11_u8; 32]).unwrap();
        std::fs::write(&replacement, [0x22_u8; 32]).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .access_mode(GENERIC_READ | DELETE)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .unwrap();

        assert!(
            std::fs::rename(&path, &moved).is_err(),
            "the validated handle must deny path replacement"
        );
        assert!(
            std::fs::remove_file(&path).is_err(),
            "the validated handle must deny pathname deletion before disposition is armed"
        );
        mark_delete_on_close(&file).unwrap();
        drop(file);

        assert!(!path.exists());
        assert!(!moved.exists());
        assert_eq!(std::fs::read(&replacement).unwrap(), [0x22_u8; 32]);
        std::fs::remove_file(replacement).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
