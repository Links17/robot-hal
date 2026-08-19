use std::io;
use std::ptr::NonNull;

#[cfg(unix)]
use std::ffi::CString;

#[cfg(unix)]
pub(crate) struct Mapping {
    address: NonNull<u8>,
    length: usize,
    fd: libc::c_int,
    lock_fd: libc::c_int,
}

#[cfg(unix)]
impl Mapping {
    pub(crate) fn create(name: &str, length: usize) -> io::Result<Self> {
        let name = CString::new(name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared-memory name contains an interior NUL",
            )
        })?;
        // SAFETY: `name` is a NUL-terminated POSIX shm name; O_EXCL prevents aliasing an
        // existing object; the upstream shm_open contract returns an owned fd on success.
        let fd = unsafe {
            libc::shm_open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                (libc::S_IRUSR | libc::S_IWUSR) as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` owns the newly created POSIX shared-memory object; fchmod changes only
        // that object's mode. The focused create/reopen test covers this protected creation path.
        if unsafe { libc::fchmod(fd, (libc::S_IRUSR | libc::S_IWUSR) as libc::mode_t) } != 0
            && !cfg!(target_vendor = "apple")
        {
            let error = io::Error::last_os_error();
            // SAFETY: both resources are still exclusively owned on this error path.
            unsafe {
                libc::close(fd);
                libc::shm_unlink(name.as_ptr());
            }
            return Err(error);
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `stat` points to writable storage and `fd` is owned by this function.
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            let error = io::Error::last_os_error();
            // SAFETY: both resources are still exclusively owned on this error path.
            unsafe {
                libc::close(fd);
                libc::shm_unlink(name.as_ptr());
            }
            return Err(error);
        }
        // SAFETY: fstat returned success and initialized `stat`.
        let stat = unsafe { stat.assume_init() };
        if stat.st_uid != unsafe { libc::geteuid() }
            || (stat.st_mode & 0o777) != (libc::S_IRUSR | libc::S_IWUSR) as libc::mode_t
        {
            // SAFETY: both resources are still exclusively owned on this error path.
            unsafe {
                libc::close(fd);
                libc::shm_unlink(name.as_ptr());
            }
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "shared-memory object ownership or mode verification failed",
            ));
        }
        // SAFETY: `fd` was returned by shm_open and `length` is bounded by
        // layout validation and representable as off_t on supported targets.
        if unsafe { libc::ftruncate(fd, length as libc::off_t) } != 0 {
            let error = io::Error::last_os_error();
            // SAFETY: `fd` is still owned here and close does not retain it.
            unsafe { libc::close(fd) };
            // SAFETY: `name` remains valid throughout this synchronous call.
            unsafe { libc::shm_unlink(name.as_ptr()) };
            return Err(error);
        }
        // SAFETY: `fd` owns an object resized to `length`; requested range is
        // non-zero and bounded; mmap returns a distinct mapping or MAP_FAILED.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if address == libc::MAP_FAILED {
            let error = io::Error::last_os_error();
            // SAFETY: see preceding fd and name ownership invariants.
            unsafe { libc::close(fd) };
            // SAFETY: see preceding name validity invariant.
            unsafe { libc::shm_unlink(name.as_ptr()) };
            return Err(error);
        }
        // SAFETY: mmap returned a non-null, non-MAP_FAILED valid base address.
        let address: NonNull<u8> = unsafe { NonNull::new_unchecked(address.cast()) };
        let lock_fd = match create_lock_file(name.as_c_str()) {
            Ok(lock_fd) => lock_fd,
            Err(error) => {
                // SAFETY: this function still owns the mapping, descriptor, and object name.
                unsafe {
                    libc::munmap(address.as_ptr().cast(), length);
                    libc::close(fd);
                    libc::shm_unlink(name.as_ptr());
                }
                return Err(error);
            }
        };
        Ok(Self {
            address,
            length,
            fd,
            lock_fd,
        })
    }

    pub(crate) fn open_read_only(name: &str, length: usize) -> io::Result<Self> {
        let name = CString::new(name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared-memory name contains an interior NUL",
            )
        })?;
        // SAFETY: `name` is a validated NUL-terminated POSIX shm name. POSIX returns an owned
        // fd on success; the reader reopen test covers this.
        let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `stat` points to writable storage and `fd` is the owned reopened object.
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            let error = io::Error::last_os_error();
            // SAFETY: `fd` is owned and close does not retain it.
            unsafe { libc::close(fd) };
            return Err(error);
        }
        // SAFETY: fstat returned success and initialized `stat`.
        let stat = unsafe { stat.assume_init() };
        if stat.st_size < 0
            || usize::try_from(stat.st_size)
                .ok()
                .is_none_or(|size| size < length)
        {
            // SAFETY: `fd` is owned and close does not retain it.
            unsafe { libc::close(fd) };
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "shared-memory object length does not match descriptor",
            ));
        }
        // SAFETY: `fd` owns a readable POSIX shm object and layout validation
        // supplied a bounded length. mmap returns a mapping or MAP_FAILED.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if address == libc::MAP_FAILED {
            let error = io::Error::last_os_error();
            // SAFETY: `fd` is owned and close does not retain it.
            unsafe { libc::close(fd) };
            return Err(error);
        }
        // SAFETY: mmap returned a non-null, non-MAP_FAILED valid base address.
        let address: NonNull<u8> = unsafe { NonNull::new_unchecked(address.cast()) };
        let lock_fd = match open_lock_file(name.as_c_str()) {
            Ok(lock_fd) => lock_fd,
            Err(error) => {
                // SAFETY: this function still owns the reopened mapping and descriptor.
                unsafe {
                    libc::munmap(address.as_ptr().cast(), length);
                    libc::close(fd);
                }
                return Err(error);
            }
        };
        Ok(Self {
            address,
            length,
            fd,
            lock_fd,
        })
    }

    pub(crate) fn as_ptr(&self) -> *mut u8 {
        self.address.as_ptr()
    }

    pub(crate) fn try_lock_shared(&self) -> io::Result<()> {
        self.try_lock(libc::LOCK_SH | libc::LOCK_NB)
    }

    pub(crate) fn try_lock_exclusive(&self) -> io::Result<()> {
        self.try_lock(libc::LOCK_EX | libc::LOCK_NB)
    }

    pub(crate) fn unlock(&self) -> io::Result<()> {
        // SAFETY: `lock_fd` is owned by this Mapping and flock(2) releases its advisory lock
        // for this open file description without retaining Rust pointers.
        if unsafe { libc::flock(self.lock_fd, libc::LOCK_UN) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn try_lock(&self, operation: libc::c_int) -> io::Result<()> {
        // SAFETY: `lock_fd` is owned by this Mapping. BSD flock(2) specifies advisory,
        // non-blocking locks with LOCK_NB, released on process exit or final close.
        if unsafe { libc::flock(self.lock_fd, operation) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn unlink(name: &str) -> io::Result<()> {
        let name = CString::new(name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared-memory name contains an interior NUL",
            )
        })?;
        // SAFETY: name is valid and shm_unlink retains no pointer.
        let shm_result = if unsafe { libc::shm_unlink(name.as_ptr()) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            }
        } else {
            Ok(())
        };
        let lock_result = lock_path(name.as_c_str()).and_then(|lock_path| {
            // SAFETY: lock_path is valid and unlink retains no pointer.
            if unsafe { libc::unlink(lock_path.as_ptr()) } != 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::NotFound {
                    return Err(error);
                }
            }
            Ok(())
        });
        shm_result?;
        lock_result
    }
}

#[cfg(unix)]
fn lock_path(name: &std::ffi::CStr) -> io::Result<CString> {
    let name = name.to_bytes();
    const PREFIX: &[u8] = b"/seeed-hal-";
    if !name.starts_with(PREFIX)
        || name.len() != PREFIX.len() + 18
        || !name[PREFIX.len()..].iter().all(u8::is_ascii_hexdigit)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid private shared-memory mapping name",
        ));
    }
    CString::new(format!("/tmp/{}.lock", &name[1..].escape_ascii())).map_err(|_| unreachable!())
}

#[cfg(unix)]
fn create_lock_file(name: &std::ffi::CStr) -> io::Result<libc::c_int> {
    let path = lock_path(name)?;
    // SAFETY: path is NUL-terminated and O_EXCL creates a private advisory-lock inode.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            (libc::S_IRUSR | libc::S_IWUSR) as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    verify_private_lock_file(fd).map(|()| fd).inspect_err(|_| {
        // SAFETY: this function owns the descriptor on its error path.
        let _ = unsafe { libc::close(fd) };
    })
}

#[cfg(unix)]
fn open_lock_file(name: &std::ffi::CStr) -> io::Result<libc::c_int> {
    let path = lock_path(name)?;
    // SAFETY: path is NUL-terminated; O_NOFOLLOW rejects a substituted symbolic link.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    verify_private_lock_file(fd).map(|()| fd).inspect_err(|_| {
        // SAFETY: this function owns the descriptor on its error path.
        let _ = unsafe { libc::close(fd) };
    })
}

#[cfg(unix)]
fn verify_private_lock_file(fd: libc::c_int) -> io::Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `stat` is writable storage and `fd` remains owned by the caller.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat returned success and initialized stat.
    let stat = unsafe { stat.assume_init() };
    let expected_mode = (libc::S_IRUSR | libc::S_IWUSR) as libc::mode_t;
    if (stat.st_mode & libc::S_IFMT) != libc::S_IFREG
        || stat.st_uid != unsafe { libc::geteuid() }
        || (stat.st_mode & 0o777) != expected_mode
        || stat.st_nlink != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "lock file ownership, mode, or type verification failed",
        ));
    }
    Ok(())
}

#[cfg(unix)]
impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: this Mapping owns exactly the mmap range created/opened in
        // this module, and the OS does not retain it after munmap returns.
        let _ = unsafe { libc::munmap(self.address.as_ptr().cast(), self.length) };
        // SAFETY: this Mapping owns the fd and close does not retain it.
        let _ = unsafe { libc::close(self.fd) };
        // SAFETY: this Mapping owns the advisory-lock descriptor and close releases its flock.
        let _ = unsafe { libc::close(self.lock_fd) };
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::ffi::CString;
    use std::process::Command;

    use super::Mapping;

    const CHILD_MAPPING_NAME: &str = "SEEED_HAL_FLOCK_TEST_MAPPING_NAME";

    #[test]
    fn exclusive_lock_is_released_when_child_exits() {
        if let Ok(name) = std::env::var(CHILD_MAPPING_NAME) {
            let _mapping = Mapping::open_read_only(&name, 4096).unwrap();
            // SAFETY: name is a valid private mapping name and O_RDWR returns a writable
            // descriptor so this test can acquire the exclusive lock after mapping read-only.
            let name = CString::new(name).unwrap();
            let path = super::lock_path(name.as_c_str()).unwrap();
            let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
            assert!(fd >= 0);
            // SAFETY: fd is an owned writable descriptor; flock is non-blocking and the process
            // exits immediately after acquisition, intentionally bypassing cleanup.
            assert_eq!(unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) }, 0);
            // SAFETY: this subprocess intentionally simulates an abnormal process exit after
            // acquiring the inter-process lock; `_exit` avoids Rust destructor cleanup.
            unsafe { libc::_exit(0) };
        }

        let name = test_mapping_name("flock");
        let mapping = Mapping::create(&name, 4096).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("platform::tests::exclusive_lock_is_released_when_child_exits")
            .arg("--nocapture")
            .env(CHILD_MAPPING_NAME, &name)
            .status()
            .unwrap();
        assert!(status.success());

        assert!(mapping.try_lock_exclusive().is_ok());
        mapping.unlock().unwrap();
        Mapping::unlink(&name).unwrap();
    }

    #[test]
    fn rejects_non_basename_shared_memory_names() {
        for name in [
            "seeed-hal-invalid",
            "/seeed-hal/nested",
            "/seeed-hal-../escape",
        ] {
            assert!(super::lock_path(CString::new(name).unwrap().as_c_str()).is_err());
        }
    }

    #[test]
    fn lock_creation_failure_removes_shared_memory_object() {
        let name = test_mapping_name("lock-failure");
        let name_c = CString::new(name.as_str()).unwrap();
        let lock_path = super::lock_path(name_c.as_c_str()).unwrap();
        // SAFETY: this test creates an owned, unique lock path to force O_EXCL failure.
        let lock_fd = unsafe {
            libc::open(
                lock_path.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                (libc::S_IRUSR | libc::S_IWUSR) as libc::c_uint,
            )
        };
        assert!(lock_fd >= 0);

        assert!(Mapping::create(&name, 4096).is_err());

        // SAFETY: creation would fail with EEXIST if Mapping::create leaked its shm object.
        let shm_fd = unsafe {
            libc::shm_open(
                name_c.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                (libc::S_IRUSR | libc::S_IWUSR) as libc::c_uint,
            )
        };
        assert!(shm_fd >= 0);
        // SAFETY: both descriptors and their corresponding test objects are owned here.
        unsafe {
            libc::close(shm_fd);
            libc::shm_unlink(name_c.as_ptr());
            libc::close(lock_fd);
            libc::unlink(lock_path.as_ptr());
        }
    }

    #[test]
    fn refuses_symbolic_link_as_client_lock_file() {
        let name = test_mapping_name("symlink");
        let mapping = Mapping::create(&name, 4096).unwrap();
        let name_c = CString::new(name.as_str()).unwrap();
        let lock_path = super::lock_path(name_c.as_c_str()).unwrap();
        // SAFETY: the broker-owned lock path is replaced only for this isolated test.
        unsafe {
            libc::unlink(lock_path.as_ptr());
            assert_eq!(libc::symlink(c"/dev/null".as_ptr(), lock_path.as_ptr()), 0);
        }

        assert!(Mapping::open_read_only(&name, 4096).is_err());

        drop(mapping);
        Mapping::unlink(&name).unwrap();
    }

    fn test_mapping_name(label: &str) -> String {
        let mut bytes = [0_u8; 9];
        let seed = format!("{label}-{}", std::process::id());
        for (index, byte) in seed.bytes().enumerate() {
            bytes[index % bytes.len()] ^= byte;
        }
        format!(
            "/seeed-hal-{}",
            bytes.map(|byte| format!("{byte:02x}")).concat()
        )
    }
}

#[cfg(windows)]
pub(crate) struct Mapping {
    address: NonNull<u8>,
    length: usize,
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl Mapping {
    pub(crate) fn create(name: &str, length: usize) -> io::Result<Self> {
        let wide_name = wide_name(name)?;
        let (descriptor, mut attributes) = protected_attributes()?;
        let (high, low) = split_section_length(length)?;
        // SAFETY: `attributes` and its LocalAlloc-owned descriptor remain live for this
        // synchronous call; `wide_name` is NUL-terminated; `length` was checked and split.
        let handle = unsafe {
            windows_sys::Win32::System::Memory::CreateFileMappingW(
                windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE,
                (&raw mut attributes).cast(),
                windows_sys::Win32::System::Memory::PAGE_READWRITE,
                high,
                low,
                wide_name.as_ptr(),
            )
        };
        let _descriptor = descriptor;
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: reads this thread's last-error value immediately after CreateFileMappingW.
        if unsafe { windows_sys::Win32::Foundation::GetLastError() }
            == windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS
        {
            // SAFETY: even on collision CreateFileMappingW returned an owned handle.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "named shared mapping already exists",
            ));
        }
        map_owned(
            handle,
            length,
            windows_sys::Win32::System::Memory::FILE_MAP_ALL_ACCESS,
        )
    }

    pub(crate) fn open_read_only(name: &str, length: usize) -> io::Result<Self> {
        if length == 0 {
            return Err(invalid_section_length());
        }
        let wide_name = wide_name(name)?;
        // SECTION_QUERY is required only to validate the section length with NtQuerySection before
        // mapping. The view itself is always FILE_MAP_READ; this path never requests write or
        // all-access mapping permissions.
        // SAFETY: `wide_name` is NUL-terminated and the API returns an owned section handle.
        let handle = unsafe {
            windows_sys::Win32::System::Memory::OpenFileMappingW(
                windows_sys::Win32::System::Memory::FILE_MAP_READ
                    | windows_sys::Win32::System::Memory::SECTION_QUERY,
                0,
                wide_name.as_ptr(),
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let section_length = section_length(handle);
        match validate_section_length(section_length, length) {
            Ok(()) => {}
            Err(error) => {
                // SAFETY: OpenFileMappingW returned this owned handle.
                unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
                return Err(error);
            }
        }
        map_owned(
            handle,
            length,
            windows_sys::Win32::System::Memory::FILE_MAP_READ,
        )
    }

    pub(crate) fn as_ptr(&self) -> *mut u8 {
        self.address.as_ptr()
    }

    pub(crate) fn try_lock_shared(&self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "unavailable"))
    }

    pub(crate) fn try_lock_exclusive(&self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "unavailable"))
    }

    pub(crate) fn unlock(&self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "unavailable"))
    }

    pub(crate) fn unlink(_name: &str) -> io::Result<()> {
        Ok(())
    }

    #[cfg(test)]
    fn dacl_sddl_for_test(&self) -> io::Result<String> {
        use windows_permissions::constants::SecurityInformation;
        use windows_permissions::{WindowsSecure, wrappers};

        let descriptor =
            BorrowedMappingHandle(self.handle).security_descriptor(SecurityInformation::Dacl)?;
        Ok(
            wrappers::ConvertSecurityDescriptorToStringSecurityDescriptor(
                &descriptor,
                SecurityInformation::Dacl,
            )?
            .to_string_lossy()
            .into_owned(),
        )
    }

    #[cfg(test)]
    fn trustees_for_test(&self) -> io::Result<std::collections::BTreeSet<String>> {
        use windows_permissions::WindowsSecure;
        use windows_permissions::constants::SecurityInformation;

        let descriptor =
            BorrowedMappingHandle(self.handle).security_descriptor(SecurityInformation::Dacl)?;
        let dacl = descriptor
            .dacl()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "section has no DACL"))?;
        (0..dacl.len())
            .map(|index| {
                dacl.get_ace(index)
                    .and_then(|ace| ace.sid())
                    .map(|sid| sid.to_string())
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid DACL ACE"))
            })
            .collect()
    }

    #[cfg(test)]
    fn dacl_ace_count_for_test(&self) -> io::Result<usize> {
        use windows_permissions::WindowsSecure;
        use windows_permissions::constants::SecurityInformation;

        let descriptor =
            BorrowedMappingHandle(self.handle).security_descriptor(SecurityInformation::Dacl)?;
        descriptor
            .dacl()
            .map(|dacl| dacl.len() as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "section has no DACL"))
    }

    #[cfg(test)]
    fn view_protection_for_test(
        &self,
    ) -> io::Result<windows_sys::Win32::System::Memory::PAGE_PROTECTION_FLAGS> {
        let mut information = std::mem::MaybeUninit::<
            windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION,
        >::zeroed();
        // SAFETY: `address` is a live mapped view and `information` is valid writable storage.
        let queried = unsafe {
            windows_sys::Win32::System::Memory::VirtualQuery(
                self.address.as_ptr().cast(),
                information.as_mut_ptr(),
                std::mem::size_of::<windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION>(),
            )
        };
        if queried == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a nonzero result from VirtualQuery initialized `information`.
        Ok(unsafe { information.assume_init() }.Protect)
    }
}

#[cfg(windows)]
fn protected_attributes() -> io::Result<(
    windows_permissions::LocalBox<windows_permissions::SecurityDescriptor>,
    windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
)> {
    use std::mem::size_of;

    let sid = windows_permissions::utilities::current_process_sid()?;
    let sddl = format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)(A;;GA;;;BA)");
    let descriptor =
        sddl.parse::<windows_permissions::LocalBox<windows_permissions::SecurityDescriptor>>()?;
    let attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_ptr().cast(),
        bInheritHandle: 0,
    };
    Ok((descriptor, attributes))
}

#[cfg(windows)]
fn wide_name(name: &str) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    if name.encode_utf16().any(|unit| unit == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shared-memory name contains an interior NUL",
        ));
    }
    Ok(std::ffi::OsStr::new(name)
        .encode_wide()
        .chain(Some(0))
        .collect())
}

#[cfg(windows)]
fn split_section_length(length: usize) -> io::Result<(u32, u32)> {
    let length = u64::try_from(length).map_err(|_| invalid_section_length())?;
    if length == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shared-memory section length must be non-zero",
        ));
    }
    Ok(((length >> 32) as u32, length as u32))
}

#[cfg(windows)]
fn invalid_section_length() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "shared-memory section length does not match descriptor",
    )
}

#[cfg(windows)]
fn validate_section_length(section_length: io::Result<usize>, requested: usize) -> io::Result<()> {
    match section_length {
        Ok(actual) if actual >= requested => Ok(()),
        Ok(_) => Err(invalid_section_length()),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn map_owned(
    handle: windows_sys::Win32::Foundation::HANDLE,
    length: usize,
    access: windows_sys::Win32::System::Memory::FILE_MAP,
) -> io::Result<Mapping> {
    // SAFETY: `handle` is exclusively owned by this function; `length` was checked and is
    // bounded; the mapping API retains neither Rust pointer and returns null on failure.
    let address =
        unsafe { windows_sys::Win32::System::Memory::MapViewOfFile(handle, access, 0, 0, length) };
    if address.Value.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: this function owns `handle` when view mapping fails.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
        return Err(error);
    }
    // SAFETY: a non-null MapViewOfFile result is a valid base for exactly `length` bytes.
    let address = unsafe { NonNull::new_unchecked(address.Value.cast()) };
    Ok(Mapping {
        address,
        length,
        handle,
    })
}

#[cfg(windows)]
fn section_length(handle: windows_sys::Win32::Foundation::HANDLE) -> io::Result<usize> {
    let mut information = SectionBasicInformation {
        base_address: std::ptr::null_mut(),
        allocation_attributes: 0,
        maximum_size: 0,
    };
    // SAFETY: `information` is writable storage of the documented SectionBasicInformation
    // layout; `handle` remains live and requests only its section metadata. See the documented
    // NtQuerySection SECTION_BASIC_INFORMATION contract.
    let status = unsafe {
        NtQuerySection(
            handle,
            0,
            (&raw mut information).cast(),
            std::mem::size_of::<SectionBasicInformation>() as u32,
            std::ptr::null_mut(),
        )
    };
    if status < 0 {
        return Err(section_query_error(status));
    }
    if information.maximum_size <= 0 {
        return Err(invalid_section_length());
    }
    usize::try_from(information.maximum_size as u64).map_err(|_| invalid_section_length())
}

#[cfg(windows)]
fn section_query_error(status: i32) -> io::Error {
    if status == windows_sys::Win32::Foundation::STATUS_ACCESS_DENIED {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "shared-memory section metadata query was denied",
        )
    } else {
        io::Error::new(
            io::ErrorKind::Other,
            "shared-memory section metadata query failed",
        )
    }
}

#[cfg(windows)]
#[repr(C)]
struct SectionBasicInformation {
    base_address: *mut std::ffi::c_void,
    allocation_attributes: u32,
    maximum_size: i64,
}

#[cfg(windows)]
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySection(
        section_handle: windows_sys::Win32::Foundation::HANDLE,
        information_class: u32,
        information: *mut std::ffi::c_void,
        information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

#[cfg(all(test, windows))]
struct BorrowedMappingHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(all(test, windows))]
impl std::os::windows::io::AsRawHandle for BorrowedMappingHandle {
    fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        self.0.cast()
    }
}

#[cfg(windows)]
impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: this Mapping owns exactly this MapViewOfFile range.
        let _ = unsafe {
            windows_sys::Win32::System::Memory::UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.address.as_ptr().cast(),
                },
            )
        };
        // SAFETY: this Mapping owns handle and CloseHandle retains no reference.
        let _ = unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
        let _ = self.length;
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::collections::BTreeSet;

    use super::{Mapping, validate_section_length};

    #[test]
    fn created_mapping_has_a_protected_three_trustee_dacl() {
        let name = test_mapping_name("dacl");
        let mapping = Mapping::create(&name, 4096).unwrap();
        let sddl = mapping.dacl_sddl_for_test().unwrap();
        assert!(sddl.starts_with("D:P"), "DACL must be protected: {sddl}");
        assert_eq!(mapping.dacl_ace_count_for_test().unwrap(), 3);
        assert_eq!(mapping.trustees_for_test().unwrap(), expected_trustees());
        drop(mapping);
        Mapping::unlink(&name).unwrap();
    }

    #[test]
    fn create_rejects_an_existing_mapping_name() {
        let name = test_mapping_name("collision");
        let first = Mapping::create(&name, 4096).unwrap();
        let error = match Mapping::create(&name, 4096) {
            Ok(_) => panic!("existing names must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        drop(first);
        Mapping::unlink(&name).unwrap();
    }

    #[test]
    fn create_rejects_a_zero_length_section() {
        let error = match Mapping::create(&test_mapping_name("zero"), 0) {
            Ok(_) => panic!("zero-length section"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn read_only_open_rejects_a_request_larger_than_the_section() {
        let name = test_mapping_name("short");
        let mapping = Mapping::create(&name, 4096).unwrap();
        let error = match Mapping::open_read_only(&name, 8192) {
            Ok(_) => panic!("larger mapping request is invalid"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        drop(mapping);
        Mapping::unlink(&name).unwrap();
    }

    #[test]
    fn query_failure_is_not_disguised_as_an_invalid_section_length() {
        let error = validate_section_length(
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "section query denied",
            )),
            4096,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn read_only_open_maps_a_read_only_view() {
        let name = test_mapping_name("read-only-view");
        let owner = Mapping::create(&name, 4096).unwrap();
        let reader = Mapping::open_read_only(&name, 4096).unwrap();

        assert_eq!(
            reader.view_protection_for_test().unwrap(),
            windows_sys::Win32::System::Memory::PAGE_READONLY
        );

        drop(reader);
        drop(owner);
        Mapping::unlink(&name).unwrap();
    }

    fn expected_trustees() -> BTreeSet<String> {
        [
            windows_permissions::utilities::current_process_sid()
                .unwrap()
                .to_string(),
            "S-1-5-18".to_owned(),
            "S-1-5-32-544".to_owned(),
        ]
        .into_iter()
        .collect()
    }

    fn test_mapping_name(label: &str) -> String {
        format!(
            "Local\\seeed-hal-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) struct Mapping;

#[cfg(not(any(unix, windows)))]
impl Mapping {
    pub(crate) fn create(_name: &str, _length: usize) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "named shared memory is unavailable on this platform",
        ))
    }
    pub(crate) fn open_read_only(_name: &str, _length: usize) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "named shared memory is unavailable on this platform",
        ))
    }
    pub(crate) fn as_ptr(&self) -> *mut u8 {
        std::ptr::null_mut()
    }
    pub(crate) fn try_lock_shared(&self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "unavailable"))
    }
    pub(crate) fn try_lock_exclusive(&self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "unavailable"))
    }
    pub(crate) fn unlock(&self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "unavailable"))
    }
    pub(crate) fn unlink(_name: &str) -> io::Result<()> {
        Ok(())
    }
}
