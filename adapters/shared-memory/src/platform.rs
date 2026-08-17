use std::io;
use std::ptr::NonNull;

#[cfg(unix)]
use std::ffi::CString;

#[cfg(unix)]
pub(crate) struct Mapping {
    address: NonNull<u8>,
    length: usize,
    fd: libc::c_int,
    semaphore: *mut libc::sem_t,
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
        // SAFETY: `name` is a NUL-terminated POSIX shm name; O_EXCL prevents
        // aliasing an existing object; the upstream shm_open contract returns
        // an owned fd on success. Focused reopen tests cover the object flow.
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
        // non-zero and bounded; POSIX mmap returns a distinct mapping or MAP_FAILED.
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
        // SAFETY: name is a valid POSIX semaphore name. O_EXCL prevents aliasing; the semaphore
        // is a process-shared system synchronization object. Focused reopen tests cover it.
        let semaphore = unsafe {
            libc::sem_open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL,
                (libc::S_IRUSR | libc::S_IWUSR) as libc::c_uint,
                1,
            )
        };
        if semaphore == libc::SEM_FAILED {
            let error = io::Error::last_os_error();
            // SAFETY: mapping, fd, and shm object are all owned on this error path.
            unsafe {
                libc::munmap(address.as_ptr().cast(), length);
                libc::close(fd);
                libc::shm_unlink(name.as_ptr());
            }
            return Err(error);
        }
        Ok(Self {
            address,
            length,
            fd,
            semaphore,
        })
    }

    pub(crate) fn open_read_only(name: &str, length: usize) -> io::Result<Self> {
        let name = CString::new(name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared-memory name contains an interior NUL",
            )
        })?;
        // SAFETY: `name` is a validated NUL-terminated POSIX shm name. POSIX
        // returns an owned fd on success; the reader reopen test covers this.
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
        // SAFETY: name is valid and sem_open returns a process-shared semaphore handle.
        let semaphore = unsafe { libc::sem_open(name.as_ptr(), 0) };
        if semaphore == libc::SEM_FAILED {
            let error = io::Error::last_os_error();
            // SAFETY: mapping and fd are owned on this error path.
            unsafe {
                libc::munmap(address.as_ptr().cast(), length);
                libc::close(fd);
            }
            return Err(error);
        }
        Ok(Self {
            address,
            length,
            fd,
            semaphore,
        })
    }

    pub(crate) fn as_ptr(&self) -> *mut u8 {
        self.address.as_ptr()
    }

    pub(crate) fn try_lock_shared(&self) -> io::Result<()> {
        self.try_lock_exclusive()
    }

    pub(crate) fn try_lock_exclusive(&self) -> io::Result<()> {
        // SAFETY: this Mapping owns an opened process-shared semaphore; sem_trywait is
        // non-blocking and retains no Rust pointers.
        if unsafe { libc::sem_trywait(self.semaphore) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn unlock(&self) -> io::Result<()> {
        // SAFETY: matching lock acquisition on this owned semaphore makes posting valid.
        if unsafe { libc::sem_post(self.semaphore) } != 0 {
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
        // SAFETY: `name` is a NUL-terminated POSIX shm name and remains valid
        // for this synchronous call. POSIX shm_unlink retains no pointer.
        // SAFETY: name is valid; unlinking the semaphore prevents a stale synchronization
        // object from becoming associated with a newly created mapping name.
        if unsafe { libc::sem_unlink(name.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: name is valid and shm_unlink retains no pointer.
        if unsafe { libc::shm_unlink(name.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: this Mapping owns exactly the mmap range created/opened in
        // this module, and the OS does not retain it after munmap returns.
        let _ = unsafe { libc::munmap(self.address.as_ptr().cast(), self.length) };
        // SAFETY: this Mapping owns the fd and close does not retain it.
        let _ = unsafe { libc::close(self.fd) };
        // SAFETY: this Mapping owns the semaphore handle and sem_close retains no pointer.
        let _ = unsafe { libc::sem_close(self.semaphore) };
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
        let _ = (name, length);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows shared memory is unavailable until DACL and section-size verification is qualified",
        ))
        /*
        use std::ffi::OsStr;
        use std::mem;
        use std::os::windows::ffi::OsStrExt;

        use windows_permissions::{LocalBox, SecurityDescriptor};
        use windows_sys::Win32::Foundation::{
            ERROR_ALREADY_EXISTS, GetLastError, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE,
        };

        let current_user = windows_permissions::utilities::current_process_sid()?;
        let sddl = format!("D:P(A;;GA;;;{current_user})(A;;GA;;;SY)(A;;GA;;;BA)");
        let descriptor = sddl
            .parse::<LocalBox<SecurityDescriptor>>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.as_ptr().cast(),
            bInheritHandle: 0,
        };
        let wide_name: Vec<u16> = OsStr::new(name).encode_wide().chain(Some(0)).collect();
        let high = (length >> 32) as u32;
        let low = length as u32;
        // SAFETY: the protected, self-relative descriptor and attributes remain alive for this
        // synchronous API call; `wide_name` is NUL-terminated; mapping size is layout-bounded.
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                (&raw mut attributes).cast(),
                PAGE_READWRITE,
                high,
                low,
                wide_name.as_ptr(),
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: GetLastError reads only the calling thread's last-error value immediately
        // after CreateFileMappingW, as required by the Windows API collision contract.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // SAFETY: the returned handle is owned even when a name collision is reported.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "named shared mapping already exists",
            ));
        }
        // SAFETY: handle is a successful CreateFileMappingW result; length is bounded by the
        // checked layout and MapViewOfFile returns null on failure without retaining pointers.
        let address = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, length) };
        if address.Value.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: handle is owned after successful mapping creation.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Err(error);
        }
        // SAFETY: a non-null MapViewOfFile result is a valid mapping base for its requested size.
        let address = unsafe { NonNull::new_unchecked(address.Value.cast()) };
        Ok(Self {
            address,
            length,
            handle,
        })
        */
    }

    pub(crate) fn open_read_only(name: &str, length: usize) -> io::Result<Self> {
        let _ = (name, length);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows shared memory is unavailable until DACL and section-size verification is qualified",
        ))
        /*
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;

        use windows_sys::Win32::System::Memory::{FILE_MAP_READ, MapViewOfFile, OpenFileMappingW};

        let wide_name: Vec<u16> = OsStr::new(name).encode_wide().chain(Some(0)).collect();
        // SAFETY: wide_name is NUL-terminated and the API returns an owned handle on success.
        let handle = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, wide_name.as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: handle is owned and length is descriptor-validated before this call.
        let address = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, length) };
        if address.Value.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: handle is owned after OpenFileMappingW succeeds.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Err(error);
        }
        // SAFETY: non-null MapViewOfFile is a valid mapping base for requested length.
        let address = unsafe { NonNull::new_unchecked(address.Value.cast()) };
        Ok(Self {
            address,
            length,
            handle,
        })
        */
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
