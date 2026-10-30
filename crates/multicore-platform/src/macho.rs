use std::io::{self, Read, Seek, SeekFrom};

const MH_MAGIC_64: u32 = 0xfeed_facf;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const MH_EXECUTE: u32 = 2;

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "image is not a Mach-O 64-bit arm64 executable",
    )
}

/// A thin Mach-O executable for Apple silicon: the 64-bit magic, `CPU_TYPE_ARM64` and
/// `MH_EXECUTE`. Universal (fat) images are refused; the package ships thin binaries.
pub fn validate_macho64_arm64(image: &mut (impl Read + Seek)) -> io::Result<()> {
    let mut header = [0_u8; 16];
    image.seek(SeekFrom::Start(0))?;
    image.read_exact(&mut header).map_err(|_| invalid())?;
    let u32_at = |offset: usize| {
        u32::from_le_bytes(header[offset..offset + 4].try_into().expect("fixed range"))
    };
    if u32_at(0) != MH_MAGIC_64 || u32_at(4) != CPU_TYPE_ARM64 || u32_at(12) != MH_EXECUTE {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::validate_macho64_arm64;

    fn header(magic: u32, cpu: u32, kind: u32) -> Cursor<Vec<u8>> {
        let mut bytes = Vec::new();
        bytes.extend(magic.to_le_bytes());
        bytes.extend(cpu.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes.extend(kind.to_le_bytes());
        Cursor::new(bytes)
    }

    #[test]
    fn accepts_a_thin_arm64_executable_only() {
        assert!(validate_macho64_arm64(&mut header(0xfeed_facf, 0x0100_000c, 2)).is_ok());
        // x86_64, a dylib, a 32-bit image and a short file are refused.
        assert!(validate_macho64_arm64(&mut header(0xfeed_facf, 0x0100_0007, 2)).is_err());
        assert!(validate_macho64_arm64(&mut header(0xfeed_facf, 0x0100_000c, 6)).is_err());
        assert!(validate_macho64_arm64(&mut header(0xfeed_face, 0x0100_000c, 2)).is_err());
        assert!(validate_macho64_arm64(&mut Cursor::new(vec![0xcf, 0xfa])).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn this_test_binary_is_accepted() {
        let mut file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        validate_macho64_arm64(&mut file).unwrap();
    }
}
