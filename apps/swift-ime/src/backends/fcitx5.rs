//! fcitx5 backend — thin wrapper around [`ime_core_ffi`] for the binary target
//! (testing / ibus modes). The real C ABI functions live in `crates/ime-core-ffi/`
//! (the cdylib linked by the C++ glue). This module just re-exports them so tests
//! can exercise the same code path that the .so uses at runtime.
//!
//! When compiled as part of the binary (not the cdylib), the `#[no_mangle]` functions
//! from `ime_core_ffi` become regular Rust functions — they work identically.

pub use swift_ime::ffi::{
    swift_ime_activate,
    swift_ime_candidates,
    swift_ime_deactivate,
    swift_ime_init,
    swift_ime_process_key,
    swift_ime_reset,
    swift_ime_select_candidate,
};

#[cfg(test)]
mod tests {
    use super::*;
    use swift_ime::ffi::ImeActionFFI;
    use std::ffi::{CStr, CString};
    use std::os::raw::c_char;

    #[test]
    fn ffi_roundtrip_init_process_reset() {
        let path = CString::new("").unwrap();
        assert_eq!(swift_ime_init(path.as_ptr()), 0);

        // process_key → preedit
        let mut buf = vec![0u8; 256];
        let mut len: u32 = 0;
        let a = swift_ime_process_key('/' as u32, buf.as_mut_ptr(), 256, &mut len);
        assert_eq!(a, ImeActionFFI::Preedit);
        assert_eq!(
            unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) }.to_str().unwrap(),
            "/"
        );

        // reset clears the buffer
        swift_ime_reset();

        // fresh '/' after reset
        let mut buf2 = vec![0u8; 256];
        let mut len2: u32 = 0;
        let a2 = swift_ime_process_key('/' as u32, buf2.as_mut_ptr(), 256, &mut len2);
        assert_eq!(a2, ImeActionFFI::Preedit);
    }
}
