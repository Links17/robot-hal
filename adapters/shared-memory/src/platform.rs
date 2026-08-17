use std::io;
use std::ptr::NonNull;

#[cfg(unix)]
use std::ffi::CString;

#[cfg(unix)]
pub(crate) struct Mapping {
    address: NonNull<u8>,
    length: usize,
    fd: libc::c_int,
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
        let address = unsafe { NonNull::new_unchecked(address.cast()) };
        Ok(Self {
            address,
            length,
            fd,
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
        let address = unsafe { NonNull::new_unchecked(address.cast()) };
        Ok(Self {
            address,
            length,
            fd,
        })
    }

    pub(crate) fn as_ptr(&self) -> *mut u8 {
        self.address.as_ptr()
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
    }

    pub(crate) fn open_read_only(name: &str, length: usize) -> io::Result<Self> {
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
    }

    pub(crate) fn as_ptr(&self) -> *mut u8 {
        self.address.as_ptr()
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
    pub(crate) fn unlink(_name: &str) -> io::Result<()> {
        Ok(())
    }
}
