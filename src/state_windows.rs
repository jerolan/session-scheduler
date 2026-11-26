use std::{
    ffi::{c_void, OsStr},
    fs::File,
    io,
    mem::{size_of, MaybeUninit},
    os::windows::{
        ffi::OsStrExt,
        fs::MetadataExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{
        LocalFree, ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, GENERIC_READ, GENERIC_WRITE,
        INVALID_HANDLE_VALUE,
    },
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
        },
        EqualSid, GetAce, GetSecurityDescriptorControl, GetSecurityDescriptorOwner,
        GetTokenInformation, TokenUser, ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES, SE_DACL_PROTECTED, TOKEN_QUERY,
        TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, GetDriveTypeW, MoveFileExW, CREATE_NEW, FILE_ALL_ACCESS,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE, MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH, OPEN_EXISTING, READ_CONTROL,
    },
    System::{
        Threading::{GetCurrentProcess, OpenProcessToken},
        WindowsProgramming::DRIVE_FIXED,
    },
};

// State paths stay pinned until the process releases StateAccess. No checked directory
// can be moved or replaced while we use a path beneath it.
pub struct StateAccess {
    descriptor: Vec<u32>,
    _directories: Vec<File>,
}

struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // The Win32 security APIs allocate these buffers with LocalAlloc.
        unsafe { LocalFree(self.0) };
    }
}

fn wide(path: &OsStr) -> io::Result<Vec<u16>> {
    let mut buffer: Vec<_> = path.encode_wide().collect();
    if buffer.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "state path contains NUL",
        ));
    }
    buffer.push(0);
    Ok(buffer)
}

fn check(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn permission_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "state requires an owner-only protected ACL and ordinary files",
    )
}

fn owner_descriptor() -> io::Result<Vec<u32>> {
    let mut token = null_mut();
    // The successful call transfers an owned process-token handle.
    unsafe {
        check(OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY,
            &mut token,
        ))?
    };
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut size = 0;
    // A null buffer obtains the byte count required for TOKEN_USER and its SID.
    let first =
        unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut size) };
    if first != 0
        || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
        || (size as usize) < size_of::<TOKEN_USER>()
    {
        return Err(io::Error::other("could not size Windows account SID"));
    }
    let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    unsafe {
        check(GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        ))?;
    }
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = null_mut();
    unsafe { check(ConvertSidToStringSidW(user.User.Sid, &mut sid))? };
    let _sid_alloc = LocalAllocation(sid.cast());
    // The converted SID is a null-terminated UTF-16 string owned by _sid_alloc.
    let sid = unsafe {
        let mut len = 0;
        while *sid.add(len) != 0 {
            len += 1;
        }
        String::from_utf16(std::slice::from_raw_parts(sid, len)).map_err(io::Error::other)?
    };
    let sddl: Vec<_> = format!("O:{sid}D:P(A;;FA;;;{sid})")
        .encode_utf16()
        .chain([0])
        .collect();
    let mut descriptor = null_mut();
    let mut length = 0;
    // This API returns a self-relative descriptor, which can be copied into aligned owned memory.
    unsafe {
        check(ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            &mut length,
        ))?;
    }
    let _descriptor_alloc = LocalAllocation(descriptor);
    let mut bytes = vec![0u32; (length as usize).div_ceil(size_of::<u32>())];
    unsafe {
        std::ptr::copy_nonoverlapping(
            descriptor.cast::<u8>(),
            bytes.as_mut_ptr().cast::<u8>(),
            length as usize,
        );
    }
    Ok(bytes)
}

fn open_file(
    path: &Path,
    access: u32,
    creation: u32,
    directory: bool,
    security: *const SECURITY_ATTRIBUTES,
) -> io::Result<File> {
    let path = wide(path.as_os_str())?;
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | if directory {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            0
        };
    // OPEN_REPARSE_POINT applies to the final path component, so each ancestor is opened separately.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            security,
            creation,
            flags,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_handle(handle) };
    let attributes = file.metadata()?.file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (attributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
    {
        return Err(permission_error());
    }
    Ok(file)
}

impl StateAccess {
    pub fn open(path: &Path) -> io::Result<(PathBuf, Self)> {
        let mut parts = path.components();
        let drive = match parts.next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
                _ => return Err(permission_error()),
            },
            _ => return Err(permission_error()),
        };
        if parts.next() != Some(Component::RootDir) {
            return Err(permission_error());
        }
        let mut parts = parts.peekable();
        if parts.peek().is_none() {
            return Err(permission_error());
        }
        let drive_letter = char::from(drive);
        let root = wide(OsStr::new(&format!(r"{}:\", drive_letter)))?;
        // A DOS drive letter can also identify a mapped network drive.
        if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows state must be on a fixed local drive",
            ));
        }
        let mut directory = PathBuf::from(format!(r"\\?\{}:\", drive_letter));
        let mut state = Self {
            descriptor: owner_descriptor()?,
            _directories: vec![open_file(&directory, 0, OPEN_EXISTING, true, null())?],
        };
        while let Some(part) = parts.next() {
            let Component::Normal(name) = part else {
                return Err(permission_error());
            };
            // Verbatim names are used throughout, including checks and replacement.
            directory.push(name);
            if parts.peek().is_none() {
                let name = wide(directory.as_os_str())?;
                if let Err(error) =
                    unsafe { check(CreateDirectoryW(name.as_ptr(), &state.attributes())) }
                {
                    if error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
                        return Err(error);
                    }
                }
                let handle = open_file(&directory, READ_CONTROL, OPEN_EXISTING, true, null())?;
                state.verify(&handle)?;
                state._directories.push(handle);
            } else {
                state
                    ._directories
                    .push(open_file(&directory, 0, OPEN_EXISTING, true, null())?);
            }
        }
        Ok((directory, state))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor.as_ptr().cast_mut().cast(),
            bInheritHandle: 0,
        }
    }

    fn verify(&self, file: &File) -> io::Result<()> {
        let mut expected: PSID = null_mut();
        let mut owner: PSID = null_mut();
        let mut acl = null_mut();
        let mut actual = null_mut();
        // Read the descriptor from the same open object whose attributes were checked.
        unsafe {
            let mut owner_defaulted = 0;
            check(GetSecurityDescriptorOwner(
                self.descriptor.as_ptr().cast_mut().cast(),
                &mut expected,
                &mut owner_defaulted,
            ))?;
            let status = GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut actual,
            );
            if status != 0 {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            let _actual_alloc = LocalAllocation(actual);
            let mut control = MaybeUninit::<u16>::uninit();
            let mut revision = 0;
            check(GetSecurityDescriptorControl(
                actual,
                control.as_mut_ptr(),
                &mut revision,
            ))?;
            if owner.is_null()
                || EqualSid(owner, expected) == 0
                || acl.is_null()
                || control.assume_init() & SE_DACL_PROTECTED == 0
                || (*acl).AceCount != 1
            {
                return Err(permission_error());
            }
            let mut ace = null_mut();
            check(GetAce(acl, 0, &mut ace))?;
            let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
            // ACCESS_ALLOWED_ACE_TYPE is zero. Inspect the common header before its SID or mask.
            if (*ace).Header.AceType != 0
                || (*ace).Header.AceFlags != 0
                || (*ace).Mask != FILE_ALL_ACCESS
                || EqualSid(
                    std::ptr::addr_of!((*ace).SidStart).cast_mut().cast(),
                    expected,
                ) == 0
            {
                return Err(permission_error());
            }
        }
        Ok(())
    }

    pub fn read(&self, path: &Path) -> io::Result<File> {
        let file = open_file(
            path,
            GENERIC_READ | READ_CONTROL,
            OPEN_EXISTING,
            false,
            null(),
        )?;
        self.verify(&file)?;
        Ok(file)
    }

    pub fn create(&self, path: &Path) -> io::Result<File> {
        let file = open_file(
            path,
            GENERIC_WRITE | READ_CONTROL,
            CREATE_NEW,
            false,
            &self.attributes(),
        )?;
        self.verify(&file)?;
        Ok(file)
    }

    pub fn replace(&self, source: &Path, destination: &Path) -> io::Result<()> {
        match self.read(destination) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let source = wide(source.as_os_str())?;
        let destination = wide(destination.as_os_str())?;
        // A same-directory replacement retains the protected descriptor of the source file.
        unsafe {
            check(MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            ))
        }
    }
}
