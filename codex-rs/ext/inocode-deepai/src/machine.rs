//! The machine identifier sent as `X-DeepAI-Code-Machine`: the SHA-256 (hex) of an identifier the
//! operating system keeps for this machine. The identifier itself is never sent or stored.
//!
//! Windows: the SMBIOS UUID, else `MachineGuid`. macOS: `IOPlatformUUID`. Linux:
//! `/etc/machine-id` (the server's acceptance of this one is not confirmed). When nothing can be
//! read, there is no header.

use sha2::Digest;
use std::sync::OnceLock;

/// The hashed machine identifier, read once per process.
pub fn machine_hash() -> Option<&'static str> {
    static HASH: OnceLock<Option<String>> = OnceLock::new();
    HASH.get_or_init(|| raw_machine_id().map(|raw| hash_of(&raw)))
        .as_deref()
}

pub(crate) fn hash_of(raw: &str) -> String {
    crate::hex(&sha2::Sha256::digest(raw.trim().as_bytes()))
}

/// Placeholder UUIDs that many boards report: all zeros, all `F`, or a repeated byte.
pub(crate) fn is_placeholder_uuid(uuid: &str) -> bool {
    let digits: Vec<char> = uuid
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    digits.is_empty() || digits.iter().all(|c| *c == digits[0])
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
        let text = std::fs::read_to_string("/etc/machine-id").ok()?;
        let id = text.trim().to_string();
        (!id.is_empty()).then_some(id)
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
    let smbios = run(
        "powershell",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-CimInstance -ClassName Win32_ComputerSystemProduct).UUID",
        ],
    )
    .map(|text| text.trim().to_string())
    .filter(|uuid| !uuid.is_empty() && !is_placeholder_uuid(uuid));
    if smbios.is_some() {
        return smbios;
    }
    let guid = run(
        "reg",
        &[
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ],
    )?;
    machine_guid_from_reg_output(&guid)
}

#[cfg(any(windows, test))]
pub(crate) fn machine_guid_from_reg_output(output: &str) -> Option<String> {
    output
        .lines()
        .find(|line| line.contains("MachineGuid"))
        .and_then(|line| line.split_whitespace().last())
        .map(str::to_string)
        .filter(|guid| guid != "MachineGuid" && !guid.is_empty())
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

#[cfg(any(target_os = "macos", test))]
pub(crate) fn platform_uuid_from_ioreg(output: &str) -> Option<String> {
    let line = output.lines().find(|line| line.contains("IOPlatformUUID"))?;
    let value = line.split('=').nth(1)?.trim().trim_matches('"').trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_sha256_hex_of_the_trimmed_identifier() {
        let hash = hash_of(" abc \n");
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, hash_of("abc"));
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn placeholder_uuids_are_recognised() {
        assert!(is_placeholder_uuid("00000000-0000-0000-0000-000000000000"));
        assert!(is_placeholder_uuid("FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF"));
        assert!(!is_placeholder_uuid("4C4C4544-0035-5110-8036-B8C04F4D4B32"));
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
            Some("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE")
        );
    }
}
