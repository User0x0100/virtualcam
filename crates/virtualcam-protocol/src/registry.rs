//! Registry schema shared by the manager and the sources.
//!
//! ```text
//! {HKCU|HKLM}\Software\VirtualCamRs\Cameras\{ID}
//!     Backend       REG_DWORD   BACKEND_*
//!     Name          REG_SZ      friendly name
//!     AppId         REG_SZ      optional owning-application tag
//!     Format        REG_BINARY  WireVideoFormat: DirectShow idle default /
//!                               Media Foundation immutable native mode
//!     SourcePath    REG_SZ      DirectShow: registered source DLL
//!     RendezvousId  REG_SZ      Media Foundation: pipe routing id
//!     SymbolicLink  REG_SZ      Media Foundation: device symbolic link
//!
//! HKCU\Software\VirtualCamRs\History\{ID}
//!     LastFormat    REG_BINARY  DirectShow: last successfully published format
//! ```
//!
//! HKCU holds per-user cameras (DirectShow per-user scope and every Media
//! Foundation camera); HKLM holds machine-wide DirectShow cameras. The Media
//! Foundation source never reads these keys: it runs as Local Service and
//! receives its configuration through vendor attributes.

/// Camera records root, relative to HKCU or HKLM.
pub const CAMERAS_KEY: &str = "Software\\VirtualCamRs\\Cameras";
/// Per-user DirectShow format history root, relative to HKCU.
pub const HISTORY_KEY: &str = "Software\\VirtualCamRs\\History";

/// `Backend` value of a DirectShow camera.
pub const BACKEND_DIRECTSHOW: u32 = 1;
/// `Backend` value of a Media Foundation camera.
pub const BACKEND_MEDIA_FOUNDATION: u32 = 2;

/// Value names.
pub mod values {
    /// `REG_DWORD` backend code.
    pub const BACKEND: &str = "Backend";
    /// `REG_SZ` friendly name.
    pub const NAME: &str = "Name";
    /// `REG_SZ` application tag.
    pub const APP_ID: &str = "AppId";
    /// `REG_BINARY` wire format.
    pub const FORMAT: &str = "Format";
    /// `REG_SZ` DirectShow source path.
    pub const SOURCE_PATH: &str = "SourcePath";
    /// `REG_SZ` Media Foundation rendezvous id.
    pub const RENDEZVOUS_ID: &str = "RendezvousId";
    /// `REG_SZ` Media Foundation symbolic link.
    pub const SYMBOLIC_LINK: &str = "SymbolicLink";
    /// `REG_BINARY` last published DirectShow format.
    pub const LAST_FORMAT: &str = "LastFormat";
}
