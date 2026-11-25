//! One opt-in, payload-free ETW event after a local source review. No session
//! is started here. The OS supplies the timestamp and process/thread IDs.

use super::{Assessment, Gap};

/// This API has no parameter for source, findings, file names or target PIDs.
/// `Observed(())` means ETW accepted the write, not that a listener retained it.
pub fn submit_review_completion() -> Assessment<()> {
    #[cfg(windows)]
    return windows::submit();
    #[cfg(not(windows))]
    Assessment::Unverified(Gap::Unsupported)
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ptr::null;
    use windows_sys::Win32::System::Diagnostics::Etw::{
        EVENT_DESCRIPTOR, EventEnabled, EventRegister, EventUnregister, EventWrite, REGHANDLE,
    };
    use windows_sys::core::GUID;

    const PROVIDER: GUID = GUID::from_u128(0x87a9f157_e3c2_4589_93a0_299a3d325e72);
    const REVIEW_COMPLETED: EVENT_DESCRIPTOR = EVENT_DESCRIPTOR {
        Id: 1,
        Version: 1,
        Channel: 0,
        Level: 4, // informational
        Opcode: 0,
        Task: 0,
        Keyword: 1,
    };

    struct Registration(REGHANDLE);

    impl Drop for Registration {
        fn drop(&mut self) {
            // SAFETY: this wrapper owns one successfully registered handle.
            unsafe { EventUnregister(self.0) };
        }
    }

    pub(super) fn submit() -> Assessment<()> {
        let mut handle = 0;
        // SAFETY: the provider ID is static; no callback or context is registered.
        if unsafe { EventRegister(&PROVIDER, None, null(), &mut handle) } != 0 {
            return Assessment::Unverified(Gap::Unavailable);
        }
        let provider = Registration(handle);
        // EventWrite can report success when there is no enabled session.
        // SAFETY: the provider and descriptor remain valid across both calls.
        if !unsafe { EventEnabled(provider.0, &REVIEW_COMPLETED) } {
            return Assessment::Unverified(Gap::MissingEvidence);
        }
        // SAFETY: zero user data descriptors, so ETW reads no caller data.
        if unsafe { EventWrite(provider.0, &REVIEW_COMPLETED, 0, null()) } == 0 {
            Assessment::Observed(())
        } else {
            Assessment::Unverified(Gap::Unavailable)
        }
    }
}
