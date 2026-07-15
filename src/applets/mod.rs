pub mod apply_patch;
pub mod view_image;

use std::ffi::OsStr;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applet {
    ApplyPatch,
    ViewImage,
}

pub fn from_argv0(argv0: &OsStr) -> Option<Applet> {
    match Path::new(argv0).file_name().and_then(OsStr::to_str) {
        Some("apply_patch") => Some(Applet::ApplyPatch),
        Some("view_image") => Some(Applet::ViewImage),
        _ => None,
    }
}

pub fn dispatch(applet: Applet) -> i32 {
    match applet {
        Applet::ApplyPatch => apply_patch::main(),
        Applet::ViewImage => view_image::main(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_only_known_argv0_basenames() {
        assert_eq!(
            from_argv0(OsStr::new("/x/apply_patch")),
            Some(Applet::ApplyPatch)
        );
        assert_eq!(
            from_argv0(OsStr::new("view_image")),
            Some(Applet::ViewImage)
        );
        assert_eq!(from_argv0(OsStr::new("mu")), None);
        assert_eq!(from_argv0(OsStr::new("renamed-mu")), None);
    }
}
