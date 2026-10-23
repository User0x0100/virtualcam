//! SIDs, explicit security descriptors, and pipe-client identity.

use core::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW, SDDL_REVISION_1};
use windows::Win32::Security::{
    EqualSid, GetLengthSid, GetTokenInformation, IsValidSid, PSECURITY_DESCRIPTOR, PSID, RevertToSelf, SECURITY_ATTRIBUTES, SECURITY_IMPERSONATION_LEVEL, SecurityAnonymous, TOKEN_QUERY, TOKEN_USER, TokenImpersonationLevel,
    TokenUser,
};
use windows::Win32::System::Pipes::ImpersonateNamedPipeClient;
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken};
use windows_core::{PCWSTR, PWSTR};

/// A self-contained binary SID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sid(Vec<u8>);

impl Sid {
    fn from_psid(sid: PSID) -> Option<Self> {
        // SAFETY: `sid` is validated before its length is read.
        unsafe {
            if !IsValidSid(sid).as_bool() {
                return None;
            }
            let length = GetLengthSid(sid) as usize;
            Some(Self(core::slice::from_raw_parts(sid.0.cast::<u8>(), length).to_vec()))
        }
    }

    /// Parses `S-1-...` text.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let wide: Vec<u16> = text.encode_utf16().chain(core::iter::once(0)).collect();
        let mut sid = PSID::default();
        // SAFETY: NUL-terminated input; the SID is LocalFree'd below.
        unsafe { ConvertStringSidToSidW(PCWSTR(wide.as_ptr()), &mut sid) }.ok()?;
        let parsed = Self::from_psid(sid);
        // SAFETY: Allocated by ConvertStringSidToSidW.
        unsafe { LocalFree(Some(HLOCAL(sid.0))) };
        parsed
    }

    fn as_psid(&self) -> PSID {
        PSID(self.0.as_ptr().cast_mut().cast::<c_void>())
    }

    /// Canonical `S-1-...` text.
    pub(crate) fn to_text(&self) -> Option<String> {
        let mut text = PWSTR::null();
        // SAFETY: Valid SID; the string is LocalFree'd below.
        unsafe { ConvertSidToStringSidW(self.as_psid(), &mut text) }.ok()?;
        // SAFETY: NUL-terminated string from the API.
        let value = unsafe { text.to_string() }.ok();
        // SAFETY: Allocated by ConvertSidToStringSidW.
        unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
        value
    }

    pub(crate) fn equals(&self, other: &Self) -> bool {
        // SAFETY: Both are valid binary SIDs.
        unsafe { EqualSid(self.as_psid(), other.as_psid()) }.is_ok()
    }
}

struct Token(HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: Owned token handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn token_user(token: &Token) -> Option<Sid> {
    let mut buffer = [0_u64; 64];
    let mut length = 0_u32;
    // SAFETY: `buffer` is 512 aligned bytes, enough for TOKEN_USER + SID.
    unsafe { GetTokenInformation(token.0, TokenUser, Some(buffer.as_mut_ptr().cast()), core::mem::size_of_val(&buffer) as u32, &mut length) }.ok()?;
    // SAFETY: GetTokenInformation filled a TOKEN_USER.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    Sid::from_psid(user.User.Sid)
}

/// SID of the account this process runs as (Local Service inside Frame Server).
pub(crate) fn process_user() -> Option<Sid> {
    let mut handle = HANDLE::default();
    // SAFETY: Pseudo process handle; the token is owned below.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) }.ok()?;
    token_user(&Token(handle))
}

/// Identifies the client of a pipe whose first message was already read.
///
/// Impersonation is reverted on every path. Anonymous tokens are rejected;
/// identification level (what our producer requests) suffices.
pub(crate) fn pipe_client_user(pipe: HANDLE) -> Option<Sid> {
    struct Revert;
    impl Drop for Revert {
        fn drop(&mut self) {
            // SAFETY: Ends the impersonation started below.
            let _ = unsafe { RevertToSelf() };
        }
    }
    // SAFETY: `pipe` is a connected server pipe with a read message.
    unsafe { ImpersonateNamedPipeClient(pipe) }.ok()?;
    let _revert = Revert;
    let mut handle = HANDLE::default();
    // SAFETY: Opens the impersonation token as self (the service identity).
    unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut handle) }.ok()?;
    let token = Token(handle);
    let mut level = SECURITY_IMPERSONATION_LEVEL::default();
    let mut length = 0_u32;
    // SAFETY: `level` is writable storage of the queried size.
    unsafe { GetTokenInformation(token.0, TokenImpersonationLevel, Some((&raw mut level).cast()), core::mem::size_of::<SECURITY_IMPERSONATION_LEVEL>() as u32, &mut length) }.ok()?;
    if level == SecurityAnonymous {
        return None;
    }
    token_user(&token)
}

/// A security descriptor built from SDDL.
pub(crate) struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: An immutable LocalAlloc'd descriptor can be used from any thread.
unsafe impl Send for SecurityDescriptor {}

impl SecurityDescriptor {
    pub(crate) fn from_sddl(sddl: &str) -> Option<Self> {
        let wide: Vec<u16> = sddl.encode_utf16().chain(core::iter::once(0)).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: NUL-terminated SDDL; the descriptor is LocalFree'd on drop.
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(wide.as_ptr()), SDDL_REVISION_1, &mut descriptor, None) }.ok()?;
        Some(Self(descriptor))
    }

    pub(crate) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: Allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

/// Rendezvous pipe DACL: the source account gets full control; the owner
/// only read/write data, read/write attributes, read control, and
/// synchronize — no `FILE_CREATE_PIPE_INSTANCE`. Medium integrity label so
/// ordinary desktop producers can connect to a service-created pipe.
pub(crate) fn pipe_sddl(source: &Sid, owner: &Sid) -> Option<String> {
    Some(format!("D:P(A;;FA;;;{})(A;;0x120183;;;{})S:(ML;;NW;;;ME)", source.to_text()?, owner.to_text()?))
}

/// Ring mapping DACL: full access for the source account, `FILE_MAP_READ |
/// FILE_MAP_WRITE` only for the owner. File mappings take no SYNCHRONIZE.
pub(crate) fn mapping_sddl(source: &Sid, owner: &Sid) -> Option<String> {
    Some(format!("D:P(A;;0xF001F;;;{})(A;;0x6;;;{})S:(ML;;NW;;;ME)", source.to_text()?, owner.to_text()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sids_round_trip_and_compare() {
        let me = process_user().expect("process user");
        let text = me.to_text().expect("text");
        assert!(text.starts_with("S-1-"));
        assert_eq!(Sid::parse(&text), Some(me.clone()));
        assert!(me.equals(&Sid::parse(&text).expect("parse")));
        assert!(!me.equals(&Sid::parse("S-1-5-19").expect("local service")));
        assert_eq!(Sid::parse("not a sid"), None);
        assert_eq!(Sid::parse(""), None);
    }

    #[test]
    fn descriptors_build_for_pipe_and_mapping() {
        let me = process_user().expect("process user");
        let service = Sid::parse("S-1-5-19").expect("local service");
        assert!(SecurityDescriptor::from_sddl(&pipe_sddl(&service, &me).expect("sddl")).is_some());
        assert!(SecurityDescriptor::from_sddl(&mapping_sddl(&service, &me).expect("sddl")).is_some());
        assert!(SecurityDescriptor::from_sddl("D:(garbage").is_none());
    }
}
