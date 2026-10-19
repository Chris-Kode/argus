//! Compiled-in assets served to GPUI.
//!
//! Every file is embedded with `include_bytes!`, so nothing is read from disk
//! at runtime: [`Assets`] answers `svg().path(..)` lookups from a static
//! table.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

use crate::icons::IconName;

/// The eye mark's layers, drawn separately so each can take its own color.
pub(crate) const EYE_OUTLINE_PATH: &str = "icons/argus-eye-outline.svg";
/// The eye mark's iris layer.
pub(crate) const EYE_IRIS_PATH: &str = "icons/argus-eye-iris.svg";
/// The eye mark's pupil layer.
pub(crate) const EYE_PUPIL_PATH: &str = "icons/argus-eye-pupil.svg";

/// Files that are not one of the [`IconName`] icons.
const EXTRA_ASSETS: &[(&str, &[u8])] = &[
    (
        EYE_OUTLINE_PATH,
        include_bytes!("../assets/icons/argus-eye-outline.svg"),
    ),
    (
        EYE_IRIS_PATH,
        include_bytes!("../assets/icons/argus-eye-iris.svg"),
    ),
    (
        EYE_PUPIL_PATH,
        include_bytes!("../assets/icons/argus-eye-pupil.svg"),
    ),
    (
        "icons/argus-eye.svg",
        include_bytes!("../assets/icons/argus-eye.svg"),
    ),
];

/// The asset source installed with `Application::with_assets`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Assets;

impl Assets {
    fn entries() -> impl Iterator<Item = (&'static str, &'static [u8])> {
        IconName::ALL
            .iter()
            .map(|icon| (icon.path(), icon.bytes()))
            .chain(EXTRA_ASSETS.iter().copied())
    }
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(Self::entries()
            .find(|(entry, _)| *entry == path)
            .map(|(_, bytes)| Cow::Borrowed(bytes)))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Self::entries()
            .filter(|(entry, _)| entry.starts_with(path))
            .map(|(entry, _)| SharedString::new_static(entry))
            .collect())
    }
}
