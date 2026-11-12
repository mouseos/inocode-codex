//! The machine identifier sent as `X-DeepAI-Code-Machine`: the SHA-256 (hex) of an identifier the
//! operating system keeps for this machine. The identifier itself is never sent or stored.
//!
//! The rules are the ones DeepAI Code 0.3.35 follows (`src/deepai/machineId.ts` in its server
//! bundle): the identifier is lower-cased and must have the shape of a UUID (Linux: 32 hex
//! digits). Windows: the SMBIOS UUID unless it is one of three placeholders, else `MachineGuid`.
//! macOS: `IOPlatformUUID`. Linux: `/etc/machine-id`, else `/var/lib/dbus/machine-id`. When
//! nothing can be read there is no header (the server then answers metered calls with
//! `machine_required`).

use sha2::Digest;
use std::sync::OnceLock;

/// The hashed machine identifier, read once per process.
pub fn machine_hash() -> Option<&'static str> {
    static HASH: OnceLock<Option<String>> = OnceLock::new();
    HASH.get_or_init(|| raw_machine_id().map(|raw| hash_of(&raw)))
        .as_deref()
}

pub(crate) fn hash_of(raw: &str) -> String {
    crate::hex(&sha2::Sha256::digest(raw.as_bytes()))
}

/// The first UUID in `text` (8-4-4-4-12 hex digits), lower-cased.
pub(crate) fn first_uuid(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let shape = [8usize, 4, 4, 4, 12];
    'start: for start in 0..bytes.len() {
        let mut at = start;
        for (index, len) in shape.iter().enumerate() {
            if index > 0 {
                if bytes.get(at) != Some(&b'-') {
                    continue 'start;
                }
                at += 1;
            }
            for _ in 0..*len {
                if !bytes.get(at).is_some_and(u8::is_ascii_hexdigit) {
                    continue 'start;
                }
                at += 1;
            }
        }
        return Some(text[start..at].to_ascii_lowercase());
    }
    None
}

/// Placeholder SMBIOS UUIDs that some firmware ships.
pub(crate) fn is_placeholder_uuid(uuid: &str) -> bool {
    matches!(
        uuid.to_ascii_lowercase().as_str(),
        "00000000-0000-0000-0000-000000000000"
            | "ffffffff-ffff-ffff-ffff-ffffffffffff"
            | "03000200-0400-0500-0006-000700080009"
    )
}

pub(crate) fn smbios_uuid(output: &str) -> Option<String> {
    first_uuid(output).filter(|uuid| !is_placeholder_uuid(uuid))
}

/// The `MachineGuid` of a `reg query` answer.
pub(crate) fn machine_guid_from_reg_output(output: &str) -> Option<String> {
    let line = output.lines().find(|line| line.contains("MachineGuid"))?;
    let value = line.split("REG_SZ").nth(1)?.split_whitespace().next()?;
    first_uuid(value).filter(|uuid| uuid.len() == value.len())
}

/// The `IOPlatformUUID` of an `ioreg` answer.
#[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
pub(crate) fn platform_uuid_from_ioreg(output: &str) -> Option<String> {
    let line = output.lines().find(|line| line.contains("IOPlatformUUID"))?;
    let value = line.split('=').nth(1)?.trim().trim_matches('"').trim();
    first_uuid(value).filter(|uuid| uuid.len() == value.len())
}

/// A systemd machine id: 32 hex digits, not all zero.
#[cfg_attr(not(any(test, all(unix, not(target_os = "macos")))), allow(dead_code))]
pub(crate) fn linux_machine_id(file: &str) -> Option<String> {
    let id = file.trim().to_ascii_lowercase();
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) && !id.bytes().all(|b| b == b'0'))
        .then_some(id)
}

fn raw_machine_id() -> Option<String> {
    #[cfg(windows)]
    {
        windows_id()
    }
    #[cfg(target_os = "macos")]
    {
        macos_id()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        ["/etc/machine-id", "/var/lib/dbus/machine-id"]
            .iter()
            .find_map(|path| linux_machine_id(&std::fs::read_to_string(path).ok()?))
    }
}

#[cfg(windows)]
fn windows_id() -> Option<String> {
    use std::os::windows::process::CommandExt;
    const NO_WINDOW: u32 = 0x0800_0000;
    let run = |program: &str, args: &[&str]| -> Option<String> {
        let output = std::process::Command::new(program)
            .args(args)
            .creation_flags(NO_WINDOW)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    run(
        "powershell",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-CimInstance Win32_ComputerSystemProduct).UUID",
        ],
    )
    .and_then(|text| smbios_uuid(&text))
    .or_else(|| {
        run(
            "reg",
            &[
                "query",
                r"HKLM\SOFTWARE\Microsoft\Cryptography",
                "/v",
                "MachineGuid",
            ],
        )
        .and_then(|text| machine_guid_from_reg_output(&text))
    })
}

#[cfg(target_os = "macos")]
fn macos_id() -> Option<String> {
    let output = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    platform_uuid_from_ioreg(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_sha256_hex_of_the_identifier() {
        assert_eq!(
            hash_of("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn identifiers_are_lower_cased_and_must_look_like_uuids() {
        assert_eq!(
            smbios_uuid("\r\n4C4C4544-0035-5110-8036-B8C04F4D4B32\r\n").as_deref(),
            Some("4c4c4544-0035-5110-8036-b8c04f4d4b32")
        );
        for placeholder in [
            "00000000-0000-0000-0000-000000000000",
            "FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF",
            "03000200-0400-0500-0006-000700080009",
        ] {
            assert_eq!(smbios_uuid(placeholder), None, "{placeholder}");
        }
        assert_eq!(smbios_uuid("not a uuid"), None);
        assert_eq!(
            linux_machine_id("0123456789ABCDEF0123456789abcdef\n").as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(linux_machine_id("00000000000000000000000000000000"), None);
        assert_eq!(linux_machine_id("abc"), None);
    }

    #[test]
    fn the_platform_outputs_are_parsed() {
        let reg = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Cryptography\r\n    MachineGuid    REG_SZ    11111111-2222-3333-4444-555555555555\r\n";
        assert_eq!(
            machine_guid_from_reg_output(reg).as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        let ioreg = "    | \"IOPlatformUUID\" = \"AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE\"\n";
        assert_eq!(
            platform_uuid_from_ioreg(ioreg).as_deref(),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
        );
    }
}
