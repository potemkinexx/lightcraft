//! External camera profiles in the DNG Camera Profile format (`.dcp`): the colour tags of DNG
//! chapter 6 (`ColorMatrix1/2`, `ForwardMatrix1/2`, `CalibrationIlluminant1/2`,
//! `ProfileHueSatMap*`, `ProfileLookTable*`, `ProfileToneCurve`) in a TIFF-style file whose
//! header magic is `CR` (0x4352) instead of 42, with `UniqueCameraModel` naming the camera.
//!
//! Personal-fork feature: a profile the user owns is installed for its camera model
//! ([`install`]) and then replaces the colour model of that model's raws ([`for_camera`],
//! [`Dcp::apply`]). Nothing here ships a profile.

use crate::ColorData;
use crate::profile::ProfileLook;
use lightcraft_tiff::tags as t;
use lightcraft_tiff::{ByteOrder, Tiff};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

/// Largest `.dcp` read (Adobe-style profiles with 90×30×30 look tables are a few MB at most).
pub const MAX_FILE: usize = 32 << 20;

/// A parsed camera profile.
#[derive(Clone, Debug, PartialEq)]
pub struct Dcp {
    /// `UniqueCameraModel`, e.g. `NIKON Z 50`.
    pub model: String,
    /// `ProfileName`, else empty.
    pub name: String,
    /// `ProfileCopyright`, else empty.
    pub copyright: String,
    /// The profile's colour tags (white-balance fields left empty).
    pub color: ColorData,
}

/// Parse a `.dcp` file.
pub fn parse(bytes: &[u8]) -> Result<Dcp, String> {
    if bytes.len() > MAX_FILE {
        return Err(format!("{} bytes is too large for a camera profile", bytes.len()));
    }
    let order = match bytes.get(0..2) {
        Some(b"II") => ByteOrder::Little,
        Some(b"MM") => ByteOrder::Big,
        _ => return Err("not a DNG camera profile (no byte-order mark)".into()),
    };
    let magic = match bytes.get(2..4) {
        Some(m) => order.u16([m[0], m[1]]),
        None => return Err("not a DNG camera profile (file too short)".into()),
    };
    // a DCP says `CR`; a TIFF/DNG holding only profile tags (42) is accepted as well
    if magic != 0x4352 && magic != 42 {
        return Err(format!("not a DNG camera profile (magic {magic:#06x})"));
    }
    let mut tiff_bytes = bytes.to_vec();
    let fortytwo = match order {
        ByteOrder::Little => [42, 0],
        ByteOrder::Big => [0, 42],
    };
    if let Some(m) = tiff_bytes.get_mut(2..4) {
        m.copy_from_slice(&fortytwo);
    }
    let tiff = Tiff::parse(&tiff_bytes).map_err(|e| format!("damaged camera profile: {e}"))?;
    let ifd = tiff.ifds.first().ok_or("damaged camera profile: no IFD")?;
    let model = ifd.string(t::UNIQUE_CAMERA_MODEL).map(|s| s.trim().to_string()).unwrap_or_default();
    if model.is_empty() {
        return Err("the profile names no camera (UniqueCameraModel)".into());
    }
    let mut color = crate::dng::color_data(ifd, ifd, order);
    // a profile has no white balance of its own
    color.as_shot_neutral = None;
    color.as_shot_white_xy = None;
    color.analog_balance = None;
    color.camera_calibration = [None, None];
    color.profile.gain_table_map = None;
    if !crate::color::has_matrix(&color) {
        return Err("the profile has no colour matrix".into());
    }
    let finite = |m: &Option<crate::Mat3>| m.as_ref().is_none_or(|m| m.0.iter().flatten().all(|v| v.is_finite() && v.abs() < 64.0));
    if !color.color_matrix.iter().all(finite) || !color.forward_matrix.iter().all(finite) {
        return Err("the profile's colour matrices are out of range".into());
    }
    Ok(Dcp {
        model,
        name: ifd.string(t::PROFILE_NAME).map(|s| s.trim().to_string()).unwrap_or_default(),
        copyright: ifd.string(PROFILE_COPYRIGHT).map(|s| s.trim().to_string()).unwrap_or_default(),
        color,
    })
}

/// `ProfileCopyright` (DNG 1.2).
const PROFILE_COPYRIGHT: u16 = 50942;

impl Dcp {
    /// Replace a raw's colour model with this profile's: matrices, illuminants, baseline exposure
    /// offset, hue/saturation map, look table and tone curve. The file's own white balance
    /// (`AsShotNeutral`, vendor multipliers, analog balance) is kept; its per-unit
    /// `CameraCalibration` is dropped, as it belongs to the file's own profile.
    pub fn apply(&self, color: &mut ColorData) {
        let p = &self.color;
        color.illuminant = p.illuminant;
        color.color_matrix = p.color_matrix;
        color.forward_matrix = p.forward_matrix;
        color.camera_calibration = [None, None];
        // a profile's BaselineExposureOffset adds to the file's BaselineExposure
        color.baseline_exposure += p.baseline_exposure;
        color.profile = ProfileLook {
            hue_sat_map: p.profile.hue_sat_map.clone(),
            look_table: p.profile.look_table.clone(),
            tone_curve: p.profile.tone_curve.clone(),
            gain_table_map: color.profile.gain_table_map.take(),
        };
    }
}

/// Upper-cased words, so `NIKON Z 50`, `Nikon  Z 50` and `NIKON_Z_50` compare equal.
fn key(s: &str) -> String {
    s.split(|c: char| c.is_whitespace() || c == '_').filter(|w| !w.is_empty()).map(str::to_uppercase).collect::<Vec<_>>().join(" ")
}

/// Whether a profile for `profile_model` (a `UniqueCameraModel`) is for the camera an image
/// names: its Exif model, else make and model joined (Adobe's unique names usually are
/// "make model", e.g. `RICOH GR III` for make `RICOH IMAGING COMPANY, LTD.`, model `GR III`).
fn matches(profile_model: &str, make: Option<&str>, model: &str) -> bool {
    let (p, m) = (key(profile_model), key(model));
    if p.is_empty() || m.is_empty() {
        return false;
    }
    if p == m {
        return true;
    }
    let first = make.and_then(|mk| mk.split_whitespace().next()).map(key).unwrap_or_default();
    !first.is_empty() && p == key(&format!("{first} {m}"))
}

fn registry() -> &'static RwLock<Vec<Arc<Dcp>>> {
    static R: OnceLock<RwLock<Vec<Arc<Dcp>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Install the active profiles (one per camera model; a later one for the same model wins).
pub fn install(list: Vec<Dcp>) {
    let mut out: Vec<Arc<Dcp>> = Vec::new();
    for d in list {
        out.retain(|x| key(&x.model) != key(&d.model));
        out.push(Arc::new(d));
    }
    *registry().write().unwrap_or_else(PoisonError::into_inner) = out;
}

/// The installed profiles.
pub fn installed() -> Vec<Arc<Dcp>> {
    registry().read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The installed profile for a camera (Exif make and model).
pub fn for_camera(make: Option<&str>, model: &str) -> Option<Arc<Dcp>> {
    registry().read().unwrap_or_else(PoisonError::into_inner).iter().find(|d| matches(&d.model, make, model)).cloned()
}

/// [`for_camera`] for a camera written as one string, Exif make and model joined by a space (as
/// the catalog keeps it): every split into make and model is tried, then the whole string.
pub fn covers_camera(camera: &str) -> bool {
    let words: Vec<&str> = camera.split_whitespace().collect();
    if words.is_empty() {
        return false;
    }
    (1..words.len()).any(|i| for_camera(Some(&words[..i].join(" ")), &words[i..].join(" ")).is_some()) || for_camera(None, camera).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal little-endian DCP: UniqueCameraModel, ProfileName, ColorMatrix1, illuminant 1.
    pub(crate) fn tiny_dcp(model: &str) -> Vec<u8> {
        let mut b = b"IIRC".to_vec();
        b.extend_from_slice(&8u32.to_le_bytes());
        let model_z = format!("{model}\0");
        let name_z = "Test Look\0".to_string();
        // entries: 50708 ASCII, 50721 SRATIONAL×9, 50778 SHORT, 50936 ASCII
        let n = 4u16;
        let ifd_len = 2 + 12 * n as usize + 4;
        let data_at = 8 + ifd_len;
        let mut data = Vec::new();
        let model_off = data_at + data.len();
        data.extend_from_slice(model_z.as_bytes());
        let matrix_off = data_at + data.len();
        for v in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]].iter().flatten() {
            data.extend_from_slice(&((*v as f64 * 10000.0) as i32).to_le_bytes());
            data.extend_from_slice(&10000i32.to_le_bytes());
        }
        let name_off = data_at + data.len();
        data.extend_from_slice(name_z.as_bytes());
        b.extend_from_slice(&n.to_le_bytes());
        let mut entry = |tag: u16, ty: u16, count: u32, value: u32| {
            b.extend_from_slice(&tag.to_le_bytes());
            b.extend_from_slice(&ty.to_le_bytes());
            b.extend_from_slice(&count.to_le_bytes());
            b.extend_from_slice(&value.to_le_bytes());
        };
        entry(50708, 2, model_z.len() as u32, model_off as u32);
        entry(50721, 10, 9, matrix_off as u32);
        entry(50778, 3, 1, 21);
        entry(50936, 2, name_z.len() as u32, name_off as u32);
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&data);
        b
    }

    #[test]
    fn parses_a_minimal_profile() {
        let d = parse(&tiny_dcp("NIKON Z 50")).unwrap();
        assert_eq!(d.model, "NIKON Z 50");
        assert_eq!(d.name, "Test Look");
        assert_eq!(d.color.illuminant[0], 21);
        assert!(d.color.color_matrix[0].is_some());
    }

    #[test]
    fn rejects_damaged_files_without_panicking() {
        let good = tiny_dcp("NIKON Z 50");
        for len in 0..good.len() {
            let _ = parse(&good[..len]);
        }
        assert!(parse(b"MMRC").is_err());
        assert!(parse(b"hello world").is_err());
        let mut no_model = good.clone();
        no_model[10..12].copy_from_slice(&1u16.to_le_bytes()); // tag 50708 → 1: ignored entry
        assert!(parse(&no_model).is_err());
    }

    #[test]
    fn matches_camera_names() {
        assert!(matches("NIKON Z 50", Some("NIKON CORPORATION"), "NIKON Z 50"));
        assert!(matches("RICOH GR III", Some("RICOH IMAGING COMPANY, LTD."), "GR III"));
        assert!(matches("Canon EOS R5", None, "CANON  EOS_R5"));
        assert!(!matches("NIKON Z 50", Some("NIKON CORPORATION"), "NIKON Z 5"));
        assert!(!matches("", None, "X"));
    }

    #[test]
    fn registry_finds_profiles_by_camera() {
        install(vec![parse(&tiny_dcp("TESTCAM ONE")).unwrap()]);
        assert!(for_camera(Some("TESTCAM CO"), "ONE").is_some());
        assert!(covers_camera("TESTCAM CO TESTCAM ONE"));
        assert!(!covers_camera("OTHER CAM"));
        install(Vec::new());
        assert!(!covers_camera("TESTCAM CO TESTCAM ONE"));
    }
}
