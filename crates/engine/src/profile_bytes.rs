//! Profiles imported from bytes rather than paths (personal fork): the web app, whose files live in
//! the browser's storage. `.dcp` camera profiles, XMP looks (`crs:RGBTable`), `.cube` LUTs and zips of
//! them become the same profiles the desktop imports; the caller keeps each one's [`Stored`] copy
//! (`profiles/<stem>.dcp` or `.cube`) and hands it back to [`load_stored`] on the next start, in every
//! process that renders (the web's render workers read it themselves).
//!
//! Ids follow the stored file's stem: `dcp:<stem>` for a camera profile, `lut:<stem>` for a look.

use serde_json::{Value, json};

use crate::Session;
use crate::cmd::lut_profiles::LutProfile;

/// A profile's file to keep: `name` is the file name inside the profiles folder.
#[derive(Clone, Debug)]
pub struct Stored {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Folder of the stored profiles (inside the browser's storage).
pub const DIR: &str = "profiles";

/// Whether a file named `name` with these bytes is a profile rather than a photo or a preset: a
/// `.dcp`, a `.cube`, or an `.xmp` holding a look with a colour table.
pub fn is_profile(name: &str, bytes: &[u8]) -> bool {
    let l = name.to_ascii_lowercase();
    l.ends_with(".dcp") || l.ends_with(".cube") || (l.ends_with(".xmp") && crate::xmp_look::is_look(&String::from_utf8_lossy(bytes)))
}

fn stem(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    base.rsplit_once('.').map_or(base, |(s, _)| s).to_string()
}

/// Import one file (a zip: each profile in it). Registers what it finds in this process and adds the
/// looks to the session; returns `(report, file to keep)` per profile and `[name, reason]` per failure.
pub fn import(s: &mut Session, name: &str, bytes: &[u8]) -> (Vec<(Value, Stored)>, Vec<Value>) {
    let mut out = Vec::new();
    let mut failed = Vec::new();
    if name.to_ascii_lowercase().ends_with(".zip") {
        match crate::preset_import::read_zip(bytes) {
            Ok(entries) => {
                for (inner, data) in entries {
                    if is_profile(&inner, &data) {
                        one(s, &inner, &data, &mut out, &mut failed);
                    }
                }
            }
            Err(e) => failed.push(json!([name, e])),
        }
    } else {
        one(s, name, bytes, &mut out, &mut failed);
    }
    if out.iter().any(|(v, _)| v["kind"] == "look") {
        let _ = s.save_prefs();
        crate::profile_refs::refresh(s);
    }
    (out, failed)
}

fn one(s: &mut Session, name: &str, bytes: &[u8], out: &mut Vec<(Value, Stored)>, failed: &mut Vec<Value>) {
    let lower = name.to_ascii_lowercase();
    let folder = name.rsplit_once(['/', '\\']).and_then(|(d, _)| d.rsplit(['/', '\\']).next()).map(str::to_string);
    if lower.ends_with(".dcp") {
        match lightcraft_raw::dcp::parse(bytes) {
            Ok(d) => {
                let label = if d.name.is_empty() { stem(name) } else { d.name.clone() };
                let file_stem = crate::presets::slug(&format!("{}-{label}", d.model));
                let id = format!("dcp:{file_stem}");
                let report = json!({"kind": "camera", "id": id, "model": d.model, "name": label});
                lightcraft_raw::dcp::add_library(&id, d);
                out.push((report, Stored { name: format!("{file_stem}.dcp"), bytes: bytes.to_vec() }));
            }
            Err(e) => failed.push(json!([stem(name), e])),
        }
        return;
    }
    let text = String::from_utf8_lossy(bytes);
    let parsed = if lower.ends_with(".xmp") {
        crate::xmp_look::parse(&text, &stem(name)).map(|l| (l.name, l.group.or(folder), l.lut))
    } else {
        lightcraft_pipeline::lut::Lut3d::parse_cube(&text)
            .map(|l| (l.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| stem(name)), folder, l))
    };
    let (display, group, lut) = match parsed {
        Ok(p) => p,
        Err(e) => {
            failed.push(json!([stem(name), e]));
            return;
        }
    };
    let group = group.as_deref().and_then(crate::preset_import::group_from_dir).unwrap_or_else(|| "Imported".into());
    let file_stem = crate::presets::slug(&format!("{group}-{display}"));
    let id = format!("lut:{file_stem}");
    let cube = lut.to_cube();
    lightcraft_pipeline::lut::register(&id, lut);
    s.lut_profiles.retain(|p| p.id != id);
    s.lut_profiles.push(LutProfile { id: id.clone(), name: display.clone(), group: group.clone(), file: format!("{DIR}/{file_stem}.cube") });
    out.push((
        json!({"kind": "look", "id": id, "name": display, "group": group}),
        Stored { name: format!("{file_stem}.cube"), bytes: cube.into_bytes() },
    ));
}

/// Register a stored profile file (`<stem>.dcp` or `<stem>.cube`) in this process. Returns its id.
pub fn load_stored(name: &str, bytes: &[u8]) -> Result<String, String> {
    let st = stem(name);
    if name.ends_with(".dcp") {
        let id = format!("dcp:{st}");
        lightcraft_raw::dcp::add_library(&id, lightcraft_raw::dcp::parse(bytes)?);
        Ok(id)
    } else if name.ends_with(".cube") {
        let id = format!("lut:{st}");
        lightcraft_pipeline::lut::register(&id, lightcraft_pipeline::lut::Lut3d::parse_cube(&String::from_utf8_lossy(bytes))?);
        Ok(id)
    } else {
        Err(format!("{name}: not a stored profile"))
    }
}

/// The stored files a render with `settings` needs that this process hasn't registered yet:
/// names inside [`DIR`].
pub fn missing_for(settings: &lightcraft_develop::DevelopSettings) -> Vec<String> {
    let mut out = Vec::new();
    let camera = &settings.profile.camera;
    if let Some(st) = camera.strip_prefix("dcp:")
        && lightcraft_raw::dcp::library_get(camera).is_none()
    {
        out.push(format!("{st}.dcp"));
    }
    let look = &settings.profile.id;
    if let Some(st) = look.strip_prefix("lut:")
        && lightcraft_pipeline::lut::get(look).is_none()
    {
        out.push(format!("{st}.cube"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_files_round_trip_through_storage() {
        let mut s = Session::new();
        let cube = "TITLE \"Warm\"\nLUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n";
        assert!(is_profile("Looks/warm.cube", cube.as_bytes()));
        assert!(!is_profile("IMG_1.xmp", b"<x:xmpmeta/>"));
        let (done, failed) = import(&mut s, "Looks/warm.cube", cube.as_bytes());
        assert!(failed.is_empty(), "{failed:?}");
        let (report, stored) = &done[0];
        let id = report["id"].as_str().unwrap().to_string();
        assert!(s.lut_profiles.iter().any(|p| p.id == id));
        // another process (a render worker) registers it from the stored copy
        lightcraft_pipeline::lut::unregister(&id);
        let mut settings = lightcraft_develop::DevelopSettings::default();
        settings.profile.id = id.clone();
        assert_eq!(missing_for(&settings), vec![stored.name.clone()]);
        assert_eq!(load_stored(&stored.name, &stored.bytes).unwrap(), id);
        assert!(missing_for(&settings).is_empty());
        let (_, failed) = import(&mut s, "bad.cube", b"LUT_3D_SIZE 2\n0 0 0\n");
        assert_eq!(failed.len(), 1);
    }
}
