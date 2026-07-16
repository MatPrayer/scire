//! App-local SVG assets plus delegation to gpui-component's bundled icons.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*.svg"]
struct AppAssets;

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.is_empty() {
            return Ok(None);
        }
        if let Some(file) = AppAssets::get(path) {
            return Ok(Some(file.data));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut names: Vec<SharedString> = AppAssets::iter()
            .filter_map(|p| p.starts_with(path).then(|| p.into()))
            .collect();
        names.extend(gpui_component_assets::Assets.list(path)?);
        names.sort();
        names.dedup();
        Ok(names)
    }
}
