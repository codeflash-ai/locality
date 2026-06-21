//! Shared Windows Cloud Files path conventions.

use std::path::{Path, PathBuf};

pub fn cloud_files_mount_id_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

pub fn decode_cloud_files_mount_id_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            decoded.push((hex_value(high)? << 4) | hex_value(low)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

pub fn windows_cloud_files_registration_marker_dir(state_root: &Path, mount_id: &str) -> PathBuf {
    state_root
        .join("cloud-files")
        .join(cloud_files_mount_id_component(mount_id))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_files_mount_id_components_round_trip() {
        let mount_id = "notion/main docs!";
        let encoded = cloud_files_mount_id_component(mount_id);

        assert_eq!(encoded, "notion%2Fmain%20docs%21");
        assert_eq!(
            decode_cloud_files_mount_id_component(&encoded).as_deref(),
            Some(mount_id)
        );
    }

    #[test]
    fn invalid_cloud_files_mount_id_components_do_not_decode() {
        assert_eq!(decode_cloud_files_mount_id_component("bad%XX"), None);
    }

    #[test]
    fn cloud_files_registration_marker_paths_escape_mount_ids() {
        assert_eq!(
            windows_cloud_files_registration_marker_dir(Path::new(r"C:\State"), "notion/main"),
            PathBuf::from(r"C:\State")
                .join("cloud-files")
                .join("notion%2Fmain")
        );
    }
}
